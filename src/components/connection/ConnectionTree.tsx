import { useState, useCallback } from "react";
import { ConnectionConfig, TreeNode } from "@/types/database";
import { useConnectionStore } from "@/stores/connectionStore";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { tauriService } from "@/services/tauriService";
import { quoteIdent, isSystemDatabase } from "@/lib/utils";
import { Button } from "@/components/ui";
import { ContextMenu, MenuItem } from "@/components/ContextMenu";
import { CreateDatabaseDialog } from "@/components/connection/CreateDatabaseDialog";
import { ImportSqlDialog } from "@/components/connection/ImportSqlDialog";
import { ExportSqlDialog } from "@/components/connection/ExportSqlDialog";
import { save } from "@tauri-apps/plugin-dialog";
import { useT } from "@/i18n";
import type { MessageKey } from "@/i18n/zh-CN";
import {
  ChevronRight,
  ChevronDown,
  Database,
  Table2,
  Type,
  Hash,
  Calendar,
  Binary,
  Braces,
  ToggleLeft,
  ListOrdered,
  Key,
  Loader2,
  Plug,
  Unplug,
  Server,
  Plus,
  Trash2,
  Folder,
  Pencil,
  RefreshCw,
  Download,
  Upload,
  EyeOff,
  Eye,
  Wrench,
  Eraser,
  ShieldCheck,
  FileCode2,
  Zap,
  FileCog,
  Sigma,
} from "lucide-react";

interface ConnectionTreeProps {
  onNewConnection: () => void;
  onEditConnection: (config: ConnectionConfig) => void;
}

const COLOR_MAP: Record<string, string> = {
  gray: "#9ca3af",
  red: "#ef4444",
  orange: "#f97316",
  green: "#22c55e",
  blue: "#3b82f6",
  purple: "#a855f7",
};

interface MenuState {
  x: number;
  y: number;
  node: TreeNode;
}

/** 叶子节点类型：无子节点、不显示展开箭头（触发器/存储过程/函数点击直接打开定义） */
const LEAF_TYPES: TreeNode["type"][] = ["column", "index", "trigger", "procedure", "function"];
const isLeafType = (type: TreeNode["type"]) => LEAF_TYPES.includes(type);

/** 按列数据类型选图标（Columns 图标在 16px 下渲染像乱码，改用更直观的类型符号） */
const columnTypeIcon = (dataType?: string) => {
  const dt = (dataType || "").toLowerCase();
  if (/bool|bit/.test(dt)) return <ToggleLeft className="h-4 w-4 text-gray-500" />;
  if (/int|decimal|numeric|float|double|real|number|money/.test(dt))
    return <Hash className="h-4 w-4 text-gray-500" />;
  if (/date|time|year/.test(dt)) return <Calendar className="h-4 w-4 text-gray-500" />;
  if (/blob|binary|bytea|image/.test(dt)) return <Binary className="h-4 w-4 text-gray-500" />;
  if (/json/.test(dt)) return <Braces className="h-4 w-4 text-gray-500" />;
  return <Type className="h-4 w-4 text-gray-500" />;
};

export function ConnectionTree({ onNewConnection, onEditConnection }: ConnectionTreeProps) {
  const t = useT();
  const treeData = useConnectionStore((s) => s.treeData);
  const expandedNodes = useConnectionStore((s) => s.expandedNodes);
  const loadingNodes = useConnectionStore((s) => s.loadingNodes);
  const activeConnection = useConnectionStore((s) => s.activeConnection);
  const liveIds = useConnectionStore((s) => s.liveIds);
  const saved = useConnectionStore((s) => s.saved);
  const expandNode = useConnectionStore((s) => s.expandNode);
  const collapseNode = useConnectionStore((s) => s.collapseNode);
  const setNodeLoading = useConnectionStore((s) => s.setNodeLoading);
  const updateNodeChildren = useConnectionStore((s) => s.updateNodeChildren);
  const clearNodeChildren = useConnectionStore((s) => s.clearNodeChildren);
  const setActiveConnection = useConnectionStore((s) => s.setActiveConnection);
  const markLive = useConnectionStore((s) => s.markLive);
  const removeSaved = useConnectionStore((s) => s.removeSaved);
  const selectedDatabase = useConnectionStore((s) => s.selectedDatabase);
  const setSelectedDatabase = useConnectionStore((s) => s.setSelectedDatabase);
  const showSystemDatabases = useConnectionStore((s) => s.showSystemDatabases);
  const toggleShowSystemDatabases = useConnectionStore((s) => s.toggleShowSystemDatabases);

  const openDatabase = useWorkspaceStore((s) => s.openDatabase);
  const openTable = useWorkspaceStore((s) => s.openTable);
  const openView = useWorkspaceStore((s) => s.openView);
  const openRoutine = useWorkspaceStore((s) => s.openRoutine);
  const openCreateTable = useWorkspaceStore((s) => s.openCreateTable);
  const openUsers = useWorkspaceStore((s) => s.openUsers);
  const openServer = useWorkspaceStore((s) => s.openServer);
  const openQuery = useWorkspaceStore((s) => s.openQuery);

  const [error, setError] = useState<string | null>(null);
  const [menu, setMenu] = useState<MenuState | null>(null);
  // 新建数据库对话框的目标连接 id
  const [createDbFor, setCreateDbFor] = useState<string | null>(null);
  // SQL 导入对话框的目标库 { connId, db }
  const [importFor, setImportFor] = useState<{ connId: string; db: string } | null>(null);
  // SQL 转储（导出）对话框的目标库 { connId, db }
  const [exportFor, setExportFor] = useState<{ connId: string; db: string } | null>(null);

  const getDriverType = useCallback(
    (connectionId: string) =>
      saved.find((c) => c.id === connectionId)?.driverType ?? "mysql",
    [saved]
  );

  const loadChildren = useCallback(
    async (node: TreeNode, force = false) => {
      if (!force && (node.children || loadingNodes.has(node.id))) return;

      setNodeLoading(node.id, true);
      try {
        let children: TreeNode[] = [];

        switch (node.type) {
          case "connection": {
            const connectionId = node.id;
            const driverType = getDriverType(connectionId);
            const showSys = useConnectionStore.getState().showSystemDatabases;
            let dbs = await tauriService.getDatabases(connectionId);
            if (showSys) {
              // 显示系统库时排在业务库之后
              dbs = [...dbs].sort(
                (a, b) =>
                  Number(isSystemDatabase(a, driverType)) - Number(isSystemDatabase(b, driverType))
              );
            } else {
              dbs = dbs.filter((d) => !isSystemDatabase(d, driverType));
            }
            children = dbs.map((db) => ({
              id: `${connectionId}::db::${db}`,
              type: "database" as const,
              name: db,
            }));
            break;
          }
          case "database": {
            const parts = node.id.split("::");
            const connectionId = parts[0];
            const dbName = parts[2];
            // 表/视图/触发器/存储过程/函数一起拉；任何一类失败不阻塞其他
            const [tables, views, triggers, procedures, functions] = await Promise.all([
              tauriService.getTables(connectionId, dbName),
              tauriService.getViews(connectionId, dbName).catch(() => []),
              tauriService.getTriggers(connectionId, dbName).catch(() => []),
              tauriService.getProcedures(connectionId, dbName).catch(() => []),
              tauriService.getFunctions(connectionId, dbName).catch(() => []),
            ]);
            children = tables.map((tb) => ({
              id: `${node.id}::table::${tb.name}`,
              type: "table" as const,
              name: tb.name,
              metadata: { rowCount: tb.rowCount },
            }));
            // 有视图时追加「视图 (n)」分组节点，视图收在里面
            if (views.length > 0) {
              children.push({
                id: `${node.id}::group::views`,
                type: "folder" as const,
                name: t("tree.viewsGroup", { count: views.length }),
                metadata: { isGroup: true, groupKind: "views" },
                children: views.map((v) => ({
                  id: `${node.id}::view::${v.name}`,
                  type: "view" as const,
                  name: v.name,
                })),
              });
            }
            // 触发器 / 存储过程 / 函数分组（SQLite 无后两者，驱动返回空列表自然不显示）
            if (triggers.length > 0) {
              children.push({
                id: `${node.id}::group::triggers`,
                type: "folder" as const,
                name: t("tree.triggersGroup", { count: triggers.length }),
                metadata: { isGroup: true, groupKind: "triggers" },
                children: triggers.map((tr) => ({
                  id: `${node.id}::trigger::${tr.name}`,
                  type: "trigger" as const,
                  name: tr.name,
                  metadata: tr.comment ? { comment: tr.comment } : undefined,
                })),
              });
            }
            if (procedures.length > 0) {
              children.push({
                id: `${node.id}::group::procedures`,
                type: "folder" as const,
                name: t("tree.proceduresGroup", { count: procedures.length }),
                metadata: { isGroup: true, groupKind: "procedures" },
                children: procedures.map((p) => ({
                  id: `${node.id}::procedure::${p.name}`,
                  type: "procedure" as const,
                  name: p.name,
                  metadata: p.comment ? { comment: p.comment } : undefined,
                })),
              });
            }
            if (functions.length > 0) {
              children.push({
                id: `${node.id}::group::functions`,
                type: "folder" as const,
                name: t("tree.functionsGroup", { count: functions.length }),
                metadata: { isGroup: true, groupKind: "functions" },
                children: functions.map((f) => ({
                  id: `${node.id}::function::${f.name}`,
                  type: "function" as const,
                  name: f.name,
                  metadata: f.comment ? { comment: f.comment } : undefined,
                })),
              });
            }
            break;
          }
          case "view": {
            // 视图展开只显示列（索引/外键对视图意义不大）
            const viewParts = node.id.split("::");
            const connectionId = viewParts[0];
            const dbName = viewParts[2];
            const viewName = viewParts[viewParts.length - 1];
            const columns = await tauriService.getColumns(connectionId, dbName, viewName);
            children = [
              {
                id: `${node.id}::folder::columns`,
                type: "folder" as const,
                name: t("tree.columnsGroup", { count: columns.length }),
                children: columns.map((col) => ({
                  id: `${node.id}::col::${col.name}`,
                  type: "column" as const,
                  name: `${col.name} (${col.dataType}${col.nullable ? "" : " NOT NULL"})`,
                  metadata: { ...col } as Record<string, unknown>,
                })),
              },
            ];
            break;
          }
          case "table": {
            // 节点 id 格式: {connectionId}::db::{dbName}::table::{tableName}
            const tableParts = node.id.split("::");
            const connectionId = tableParts[0];
            const dbName = tableParts[1] === "db" ? tableParts[2] : undefined;
            const tableName = tableParts[tableParts.length - 1];

            const [columns, indexes] = await Promise.all([
              tauriService.getColumns(connectionId, dbName, tableName),
              tauriService.getIndexes(connectionId, dbName, tableName),
            ]);

            children = [
              {
                id: `${node.id}::folder::columns`,
                type: "folder" as const,
                name: t("tree.columnsGroup", { count: columns.length }),
                children: columns.map((col) => ({
                  id: `${node.id}::col::${col.name}`,
                  type: "column" as const,
                  name: `${col.name} (${col.dataType}${col.nullable ? "" : " NOT NULL"})`,
                  metadata: { ...col } as Record<string, unknown>,
                })),
              },
              {
                id: `${node.id}::folder::indexes`,
                type: "folder" as const,
                name: t("tree.indexesGroup", { count: indexes.length }),
                children: indexes.map((idx) => ({
                  id: `${node.id}::idx::${idx.name}`,
                  type: "index" as const,
                  name: idx.name,
                  metadata: { ...idx } as Record<string, unknown>,
                })),
              },
            ];
            break;
          }
        }

        updateNodeChildren(node.id, children);
        setNodeLoading(node.id, false);
      } catch (err) {
        console.error("Failed to load children:", err);
        setError(t("tree.loadFailed", { error: String(err) }));
        setNodeLoading(node.id, false);
      }
    },
    [loadingNodes, setNodeLoading, updateNodeChildren, getDriverType, t]
  );

  /** 刷新节点：清空子树并重新加载 */
  const refreshNode = useCallback(
    (node: TreeNode) => {
      clearNodeChildren(node.id);
      expandNode(node.id);
      loadChildren({ ...node, children: undefined }, true);
    },
    [clearNodeChildren, expandNode, loadChildren]
  );

  /** 切换系统库显示：递归找到所有已加载的连接节点，重建子树 */
  const handleToggleSystemDbs = useCallback(() => {
    toggleShowSystemDatabases();
    const walk = (nodes: TreeNode[]) => {
      nodes.forEach((n) => {
        if (n.type === "connection" && n.children) {
          clearNodeChildren(n.id);
          expandNode(n.id);
          loadChildren({ ...n, children: undefined }, true);
        }
        if (n.children) walk(n.children);
      });
    };
    walk(useConnectionStore.getState().treeData);
  }, [toggleShowSystemDatabases, clearNodeChildren, expandNode, loadChildren]);

  /** 点击未连接的数据库连接：用已保存配置建连，然后加载数据库列表 */
  const handleConnectSaved = useCallback(
    async (node: TreeNode) => {
      if (loadingNodes.has(node.id)) return;
      setNodeLoading(node.id, true);
      setError(null);
      try {
        await tauriService.connectSaved(node.id);
        markLive(node.id, true);
        setActiveConnection(node.id);
        expandNode(node.id);
        await loadChildren({ ...node, children: undefined });
      } catch (err) {
        console.error("Failed to connect:", err);
        setError(t("tree.connectFailed", { error: String(err) }));
        setNodeLoading(node.id, false);
      }
    },
    [loadingNodes, setNodeLoading, markLive, setActiveConnection, expandNode, loadChildren, t]
  );

  const handleDisconnect = useCallback(
    async (connectionId: string) => {
      try {
        await tauriService.disconnect(connectionId);
      } catch (err) {
        console.error("Failed to disconnect:", err);
      } finally {
        markLive(connectionId, false);
      }
    },
    [markLive]
  );

  const handleDelete = useCallback(
    async (connectionId: string) => {
      if (!window.confirm(t("tree.confirmDeleteConnection"))) return;
      try {
        await tauriService.deleteSaved(connectionId);
        removeSaved(connectionId);
      } catch (err) {
        console.error("Failed to delete:", err);
        setError(t("tree.deleteFailed", { error: String(err) }));
      }
    },
    [removeSaved, t]
  );

  const toggleExpand = useCallback(
    (node: TreeNode) => {
      if (expandedNodes.has(node.id)) {
        collapseNode(node.id);
      } else {
        expandNode(node.id);
        if (!node.children && !isLeafType(node.type)) {
          loadChildren(node);
        }
      }
    },
    [expandedNodes, expandNode, collapseNode, loadChildren]
  );

  const handleNodeClick = (node: TreeNode) => {
    switch (node.type) {
      case "connection":
        if (liveIds.has(node.id)) {
          setActiveConnection(node.id);
          openServer(node.id, node.name);
          toggleExpand(node);
        } else {
          handleConnectSaved(node);
        }
        return;
      case "database": {
        // 点击数据库：激活连接 + 设为查询上下文 + 打开库概览 tab
        const parts = node.id.split("::");
        const connId = parts[0];
        const dbName = parts[2];
        setActiveConnection(connId);
        setSelectedDatabase(dbName);
        openDatabase(connId, dbName);
        toggleExpand(node);
        return;
      }
      case "table": {
        // 点击表：打开表视图 tab（数据/结构）
        const parts = node.id.split("::");
        const connId = parts[0];
        const dbName = parts[2];
        const tableName = parts[parts.length - 1];
        setActiveConnection(connId);
        setSelectedDatabase(dbName);
        openTable(connId, dbName, tableName);
        return;
      }
      case "view": {
        // 点击视图：打开视图 tab（数据只读/定义）
        const parts = node.id.split("::");
        const connId = parts[0];
        const dbName = parts[2];
        const viewName = parts[parts.length - 1];
        setActiveConnection(connId);
        setSelectedDatabase(dbName);
        openView(connId, dbName, viewName);
        return;
      }
      case "trigger":
      case "procedure":
      case "function": {
        // 点击触发器/存储过程/函数：打开定义 tab（只读）
        const parts = node.id.split("::");
        const connId = parts[0];
        const dbName = parts[2];
        const objName = parts[parts.length - 1];
        setActiveConnection(connId);
        setSelectedDatabase(dbName);
        openRoutine(connId, dbName, node.type, objName);
        return;
      }
      case "folder":
        toggleExpand(node);
        return;
      default:
        return; // column / index 叶子节点无操作
    }
  };

  // ---------- 右键菜单 ----------

  /** 对表执行 DDL（重命名/清空/删除）并刷新库节点 */
  const runTableDdl = useCallback(
    async (node: TreeNode, sql: string, failKey: MessageKey) => {
      const parts = node.id.split("::");
      const connId = parts[0];
      const dbName = parts[2];
      try {
        await tauriService.executeQuery(connId, sql, dbName);
        const dbNodeId = `${connId}::db::${dbName}`;
        refreshNode({ id: dbNodeId, type: "database", name: dbName });
      } catch (err) {
        setError(t(failKey, { error: String(err) }));
      }
    },
    [refreshNode, t]
  );

  /** 删除数据库：DROP DATABASE 后刷新连接节点，并清除查询上下文 */
  const handleDropDatabase = useCallback(
    async (node: TreeNode) => {
      const parts = node.id.split("::");
      const connId = parts[0];
      const dbName = parts[2];
      const driverType = getDriverType(connId);
      try {
        await tauriService.executeQuery(
          connId,
          `DROP DATABASE ${quoteIdent(dbName, driverType)};`
        );
        if (activeConnection === connId && selectedDatabase === dbName) {
          setSelectedDatabase(null);
        }
        refreshNode({ id: connId, type: "connection", name: node.name });
      } catch (err) {
        setError(t("tree.dropDatabaseFailed", { error: String(err) }));
      }
    },
    [getDriverType, activeConnection, selectedDatabase, setSelectedDatabase, refreshNode, t]
  );

  const buildMenuItems = (node: TreeNode): MenuItem[] => {
    const parts = node.id.split("::");

    if (node.type === "connection") {
      const connected = liveIds.has(node.id);
      const cfg = saved.find((c) => c.id === node.id);
      const items: MenuItem[] = [];
      if (connected) {
        items.push({
          label: t("tree.serverOverview"),
          icon: <Server className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(node.id);
            openServer(node.id, node.name);
          },
        });
        items.push({
          label: t("app.newQuery"),
          icon: <FileCode2 className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(node.id);
            openQuery(node.id);
          },
        });
        // SQLite 是单文件库，新建数据库 = 新文件 = 新连接，不提供此入口
        if (cfg?.driverType !== "sqlite") {
          items.push({
            label: t("tree.newDatabase"),
            icon: <Plus className="h-3.5 w-3.5" />,
            onClick: () => setCreateDbFor(node.id),
          });
        }
        items.push({
          label: "用户与权限",
          icon: <ShieldCheck className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(node.id);
            openUsers(node.id);
          },
        });
        items.push({
          label: t("tree.disconnect"),
          icon: <Unplug className="h-3.5 w-3.5" />,
          onClick: () => handleDisconnect(node.id),
        });
        items.push({
          label: t("common.refresh"),
          icon: <RefreshCw className="h-3.5 w-3.5" />,
          onClick: () => refreshNode(node),
        });
      } else {
        items.push({
          label: t("tree.connect"),
          icon: <Plug className="h-3.5 w-3.5" />,
          onClick: () => handleConnectSaved(node),
        });
      }
      if (cfg) {
        items.push({
          label: t("tree.editConnection"),
          icon: <Pencil className="h-3.5 w-3.5" />,
          onClick: () => onEditConnection(cfg),
        });
      }
      items.push({
        label: showSystemDatabases ? t("tree.hideSystemDbs") : t("tree.showSystemDbs"),
        icon: showSystemDatabases ? (
          <EyeOff className="h-3.5 w-3.5" />
        ) : (
          <Eye className="h-3.5 w-3.5" />
        ),
        onClick: handleToggleSystemDbs,
      });
      items.push({
        label: t("tree.deleteConnection"),
        icon: <Trash2 className="h-3.5 w-3.5" />,
        danger: true,
        onClick: () => handleDelete(node.id),
      });
      return items;
    }

    if (node.type === "database") {
      const driverType = getDriverType(parts[0]);
      const sysDb = isSystemDatabase(parts[2], driverType);
      const items: MenuItem[] = [
        {
          label: t("tree.openOverview"),
          icon: <Eye className="h-3.5 w-3.5" />,
          onClick: () => openDatabase(parts[0], parts[2]),
        },
        {
          label: t("app.newQuery"),
          icon: <FileCode2 className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(parts[0]);
            setSelectedDatabase(parts[2]);
            openQuery(parts[0]);
          },
        },
        {
          label: t("common.refresh"),
          icon: <RefreshCw className="h-3.5 w-3.5" />,
          onClick: () => refreshNode(node),
        },
        {
          label: t("tree.dumpSql"),
          icon: <Download className="h-3.5 w-3.5" />,
          onClick: () => setExportFor({ connId: parts[0], db: parts[2] }),
        },
      ];
      // 系统库禁止结构变更类操作：新建表 / 导入 / 删除
      if (!sysDb) {
        items.splice(
          1,
          0,
          {
            label: t("tree.newTable"),
            icon: <Table2 className="h-3.5 w-3.5" />,
            onClick: () => {
              setActiveConnection(parts[0]);
              setSelectedDatabase(parts[2]);
              openCreateTable(parts[0], parts[2]);
            },
          }
        );
        items.push({
          label: t("tree.importSql"),
          icon: <Upload className="h-3.5 w-3.5" />,
          onClick: () => setImportFor({ connId: parts[0], db: parts[2] }),
        });
        // SQLite 的"数据库"就是文件本身，不提供删除入口
        if (driverType !== "sqlite") {
          items.push({
            label: t("tree.dropDatabase"),
            icon: <Trash2 className="h-3.5 w-3.5" />,
            danger: true,
            onClick: () => {
              if (window.confirm(t("tree.confirmDropDatabase", { name: parts[2] }))) {
                handleDropDatabase(node);
              }
            },
          });
        }
      }
      return items;
    }

    if (node.type === "table") {
      const connId = parts[0];
      const dbName = parts[2];
      const tableName = parts[parts.length - 1];
      const driverType = getDriverType(connId);
      const q = (n: string) => quoteIdent(n, driverType);
      return [
        {
          label: t("tree.viewData"),
          icon: <Eye className="h-3.5 w-3.5" />,
          onClick: () => openTable(connId, dbName, tableName),
        },
        {
          label: t("tree.newQuerySelect"),
          icon: <FileCode2 className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(connId);
            setSelectedDatabase(dbName);
            // SQL Server 用 TOP，其余用 LIMIT
            const sql =
              driverType === "sqlserver" || driverType === "mssql"
                ? `SELECT TOP 100 * FROM ${q(tableName)};`
                : `SELECT * FROM ${q(tableName)} LIMIT 100;`;
            openQuery(connId, sql);
          },
        },
        {
          label: t("tree.designTable"),
          icon: <Wrench className="h-3.5 w-3.5" />,
          onClick: () => openTable(connId, dbName, tableName), // 打开后切到结构页由用户选择
        },
        {
          label: t("tree.renameTable"),
          icon: <Pencil className="h-3.5 w-3.5" />,
          onClick: () => {
            const newName = window.prompt(t("tree.promptNewTableName"), tableName);
            if (!newName || newName === tableName) return;
            const sql =
              driverType === "postgres" || driverType === "postgresql" || driverType === "sqlite"
                ? `ALTER TABLE ${q(tableName)} RENAME TO ${q(newName)};`
                : driverType === "sqlserver" || driverType === "mssql"
                  ? `EXEC sp_rename '${tableName.replace(/'/g, "''")}', '${newName.replace(/'/g, "''")}';`
                  : `RENAME TABLE ${q(tableName)} TO ${q(newName)};`;
            runTableDdl(node, sql, "tree.renameFailed");
          },
        },
        {
          label: t("tree.exportCsv"),
          icon: <Download className="h-3.5 w-3.5" />,
          onClick: async () => {
            try {
              const path = await save({
                defaultPath: `${tableName}.csv`,
                filters: [{ name: "CSV", extensions: ["csv"] }],
              });
              if (!path) return;
              const count = await tauriService.exportTableCsv(connId, dbName, tableName, path);
              window.alert(t("tree.exportedRows", { count, path }));
            } catch (err) {
              setError(t("tree.exportFailed", { error: String(err) }));
            }
          },
        },
        {
          label: t("tree.truncateTable"),
          icon: <Eraser className="h-3.5 w-3.5" />,
          danger: true,
          onClick: () => {
            if (window.confirm(t("tree.confirmTruncate", { name: tableName }))) {
              // SQLite 没有 TRUNCATE，用 DELETE FROM 代替
              const sql =
                driverType === "sqlite"
                  ? `DELETE FROM ${q(tableName)};`
                  : `TRUNCATE TABLE ${q(tableName)};`;
              runTableDdl(node, sql, "tree.truncateFailed");
            }
          },
        },
        {
          label: t("tree.dropTable"),
          icon: <Trash2 className="h-3.5 w-3.5" />,
          danger: true,
          onClick: () => {
            if (window.confirm(t("tree.confirmDropTable", { name: tableName }))) {
              runTableDdl(node, `DROP TABLE ${q(tableName)};`, "tree.dropTableFailed");
            }
          },
        },
      ];
    }

    if (node.type === "view") {
      const connId = parts[0];
      const dbName = parts[2];
      const viewName = parts[parts.length - 1];
      const driverType = getDriverType(connId);
      const q = (n: string) => quoteIdent(n, driverType);
      return [
        {
          label: t("tree.viewData"),
          icon: <Eye className="h-3.5 w-3.5" />,
          onClick: () => openView(connId, dbName, viewName),
        },
        {
          label: t("tree.newQuerySelect"),
          icon: <FileCode2 className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(connId);
            setSelectedDatabase(dbName);
            const sql =
              driverType === "sqlserver" || driverType === "mssql"
                ? `SELECT TOP 100 * FROM ${q(viewName)};`
                : `SELECT * FROM ${q(viewName)} LIMIT 100;`;
            openQuery(connId, sql);
          },
        },
        {
          label: t("tree.exportCsv"),
          icon: <Download className="h-3.5 w-3.5" />,
          onClick: async () => {
            try {
              const path = await save({
                defaultPath: `${viewName}.csv`,
                filters: [{ name: "CSV", extensions: ["csv"] }],
              });
              if (!path) return;
              const count = await tauriService.exportTableCsv(connId, dbName, viewName, path);
              window.alert(t("tree.exportedRows", { count, path }));
            } catch (err) {
              setError(t("tree.exportFailed", { error: String(err) }));
            }
          },
        },
        {
          label: t("tree.dropView"),
          icon: <Trash2 className="h-3.5 w-3.5" />,
          danger: true,
          onClick: () => {
            if (window.confirm(t("tree.confirmDropView", { name: viewName }))) {
              runTableDdl(node, `DROP VIEW ${q(viewName)};`, "tree.dropViewFailed");
            }
          },
        },
      ];
    }

    if (node.type === "trigger" || node.type === "procedure" || node.type === "function") {
      const connId = parts[0];
      const dbName = parts[2];
      const objName = parts[parts.length - 1];
      const routineKind = node.type;
      const driverType = getDriverType(connId);
      const q = (n: string) => quoteIdent(n, driverType);
      const dropKeyword =
        routineKind === "trigger" ? "TRIGGER" : routineKind === "procedure" ? "PROCEDURE" : "FUNCTION";
      return [
        {
          label: t("tree.viewDefinition"),
          icon: <Eye className="h-3.5 w-3.5" />,
          onClick: () => {
            setActiveConnection(connId);
            setSelectedDatabase(dbName);
            openRoutine(connId, dbName, routineKind, objName);
          },
        },
        {
          label: t("tree.dropRoutine", { kind: t(`routineView.${routineKind}`) }),
          icon: <Trash2 className="h-3.5 w-3.5" />,
          danger: true,
          onClick: () => {
            if (!window.confirm(t("tree.confirmDropRoutine", { kind: t(`routineView.${routineKind}`), name: objName }))) return;
            let sql = `DROP ${dropKeyword} ${q(objName)};`;
            // PostgreSQL 删除触发器必须指定所属表（从列表加载时存的 comment 里取）
            if (routineKind === "trigger" && (driverType === "postgres" || driverType === "postgresql")) {
              const comment = node.metadata?.comment as string | undefined;
              const table = comment?.startsWith("ON ") ? comment.slice(3) : undefined;
              if (!table) {
                setError(t("tree.dropRoutineFailed", { error: "missing trigger table" }));
                return;
              }
              sql = `DROP TRIGGER ${q(objName)} ON ${q(table)};`;
            }
            runTableDdl(node, sql, "tree.dropRoutineFailed");
          },
        },
      ];
    }

    return [];
  };

  // ---------- 渲染 ----------

  const getNodeIcon = (node: TreeNode) => {
    switch (node.type) {
      case "connection": {
        const connected = liveIds.has(node.id);
        const markerColor = (node.metadata?.color as string) || "gray";
        // 圆点 = 连接状态灯（绿=已连接，灰=未连接）；服务器图标 = 颜色标记
        // 未连接时图标统一灰色，不显示标记色，避免与状态灯绿色混淆
        const dotColor = connected ? "#22c55e" : "#6b7280";
        const iconColor = connected && markerColor !== "gray" ? COLOR_MAP[markerColor] : undefined;
        return (
          <span className="flex items-center gap-1.5">
            <span
              className="w-2 h-2 rounded-full flex-shrink-0"
              style={{ backgroundColor: dotColor }}
            />
            <Server
              className={`h-4 w-4 ${iconColor ? "" : connected ? "text-orange-500" : "text-gray-300"}`}
              style={iconColor ? { color: iconColor } : undefined}
            />
          </span>
        );
      }
      case "database":
        return <Database className="h-4 w-4 text-blue-500" />;
      case "table":
        return <Table2 className="h-4 w-4 text-green-600" />;
      case "view":
        return <Eye className="h-4 w-4 text-teal-500" />;
      case "trigger":
        return <Zap className="h-4 w-4 text-amber-500" />;
      case "procedure":
        return <FileCog className="h-4 w-4 text-indigo-500" />;
      case "function":
        return <Sigma className="h-4 w-4 text-purple-500" />;
      case "column":
        if (node.metadata?.isPrimaryKey) {
          return <Key className="h-4 w-4 text-yellow-500" />;
        }
        return columnTypeIcon(node.metadata?.dataType as string | undefined);
      case "index":
        return <ListOrdered className="h-4 w-4 text-purple-500" />;
      case "folder":
        return node.metadata?.groupKind === "views" ? (
          <Eye className="h-4 w-4 text-teal-500" />
        ) : node.metadata?.groupKind === "triggers" ? (
          <Zap className="h-4 w-4 text-amber-500" />
        ) : node.metadata?.groupKind === "procedures" ? (
          <FileCog className="h-4 w-4 text-indigo-500" />
        ) : node.metadata?.groupKind === "functions" ? (
          <Sigma className="h-4 w-4 text-purple-500" />
        ) : node.metadata?.isGroup ? (
          <Folder className="h-4 w-4 text-yellow-500" />
        ) : (
          <ChevronRight className="h-4 w-4 text-gray-400" />
        );
      default:
        return null;
    }
  };

  const renderNode = (node: TreeNode, depth: number = 0) => {
    const isExpanded = expandedNodes.has(node.id);
    const isLoading = loadingNodes.has(node.id);
    const isActive =
      (node.type === "connection" && activeConnection === node.id) ||
      (node.type === "database" &&
        node.id === `${activeConnection}::db::${selectedDatabase}`);
    const paddingLeft = depth * 16 + 8;
    const connected = node.type === "connection" && liveIds.has(node.id);
    const isLeaf = isLeafType(node.type);
    const hasMenu = ["connection", "database", "table", "view", "trigger", "procedure", "function"].includes(node.type);
    const isSysDb =
      node.type === "database" &&
      isSystemDatabase(node.name, getDriverType(node.id.split("::")[0]));

    return (
      <div key={node.id}>
        <div
          className={`
            group flex items-center py-1 px-2 cursor-pointer text-sm select-none
            hover:bg-gray-100 dark:hover:bg-gray-800
            ${isActive ? "tree-node-active" : ""}
          `}
          style={{ paddingLeft }}
          onClick={() => handleNodeClick(node)}
          onContextMenu={(e) => {
            if (!hasMenu) return;
            e.preventDefault();
            setMenu({ x: e.clientX, y: e.clientY, node });
          }}
        >
          {!isLeaf && (
            <span
              className="w-4 h-4 mr-1 flex items-center justify-center"
              onClick={(e) => {
                e.stopPropagation();
                // 未连接的连接节点：点箭头直接自动建连并展开，而不是报"连接已关闭"
                if (node.type === "connection" && !liveIds.has(node.id)) {
                  handleConnectSaved(node);
                  return;
                }
                toggleExpand(node);
              }}
            >
              {isLoading ? (
                <Loader2 className="h-3 w-3 animate-spin" />
              ) : isExpanded ? (
                <ChevronDown className="h-3 w-3" />
              ) : (
                <ChevronRight className="h-3 w-3" />
              )}
            </span>
          )}
          {getNodeIcon(node)}
          <span className={`truncate ml-1.5 ${isSysDb ? "text-gray-400" : ""}`}>{node.name}</span>
          {isSysDb && (
            <span className="ml-1.5 text-[10px] px-1 rounded bg-gray-200 dark:bg-gray-700 text-gray-500 dark:text-gray-400 flex-shrink-0">
              {t("tree.system")}
            </span>
          )}
          {node.metadata?.rowCount !== undefined && node.metadata?.rowCount !== null && (
            <span className="ml-2 text-xs text-gray-400">
              {Number(node.metadata.rowCount).toLocaleString()}
            </span>
          )}
          {node.type === "connection" && (
            <span className="ml-auto opacity-0 group-hover:opacity-100 flex items-center gap-0.5">
              {connected && (
                <button
                  title={t("tree.disconnectKeep")}
                  className="hover:bg-gray-200 dark:hover:bg-gray-700 rounded p-0.5"
                  onClick={(e) => {
                    e.stopPropagation();
                    handleDisconnect(node.id);
                  }}
                >
                  <Unplug className="h-3.5 w-3.5 text-gray-400 hover:text-orange-500" />
                </button>
              )}
              <button
                title={t("tree.deleteSavedTitle")}
                className="hover:bg-gray-200 dark:hover:bg-gray-700 rounded p-0.5"
                onClick={(e) => {
                  e.stopPropagation();
                  handleDelete(node.id);
                }}
              >
                <Trash2 className="h-3.5 w-3.5 text-gray-400 hover:text-red-500" />
              </button>
            </span>
          )}
        </div>
        {isExpanded && node.children?.map((child) => renderNode(child, depth + 1))}
      </div>
    );
  };

  return (
    <div className="h-full flex flex-col border-r bg-card">
      <div className="flex items-center justify-between px-3 py-2 border-b bg-muted/50">
        <span className="font-semibold text-sm">{t("tree.title")}</span>
        <div className="flex items-center gap-0.5">
          <Button
            variant="ghost"
            size="icon"
            className="h-7 w-7"
            title={showSystemDatabases ? t("tree.hideSystemDbs") : t("tree.showSystemDbs")}
            onClick={handleToggleSystemDbs}
          >
            {showSystemDatabases ? (
              <EyeOff className="h-4 w-4 text-blue-500" />
            ) : (
              <Eye className="h-4 w-4 text-gray-400" />
            )}
          </Button>
          <Button variant="ghost" size="icon" className="h-7 w-7" onClick={onNewConnection}>
            <Plus className="h-4 w-4" />
          </Button>
        </div>
      </div>
      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200 flex items-center justify-between">
          <span className="truncate">{error}</span>
          <button onClick={() => setError(null)} className="text-red-400 hover:text-red-600 ml-2">
            ✕
          </button>
        </div>
      )}
      <div className="flex-1 overflow-auto custom-scrollbar py-1">
        {treeData.length === 0 ? (
          <div className="flex flex-col items-center justify-center h-32 text-muted-foreground text-sm">
            <Plug className="h-8 w-8 mb-2 opacity-50" />
            <p>{t("tree.noConnections")}</p>
            <p className="text-xs mt-1">{t("tree.addHint")}</p>
          </div>
        ) : (
          treeData.map((node) => renderNode(node))
        )}
      </div>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={buildMenuItems(menu.node)}
          onClose={() => setMenu(null)}
        />
      )}

      {createDbFor && (
        <CreateDatabaseDialog
          open
          connectionId={createDbFor}
          driverType={getDriverType(createDbFor)}
          onClose={() => setCreateDbFor(null)}
          onCreated={() =>
            refreshNode({ id: createDbFor, type: "connection", name: "" })
          }
        />
      )}

      {importFor && (
        <ImportSqlDialog
          open
          connectionId={importFor.connId}
          database={importFor.db}
          onClose={() => setImportFor(null)}
          onImported={() =>
            refreshNode({
              id: `${importFor.connId}::db::${importFor.db}`,
              type: "database",
              name: importFor.db,
            })
          }
        />
      )}

      {exportFor && (
        <ExportSqlDialog
          open
          connectionId={exportFor.connId}
          database={exportFor.db}
          onClose={() => setExportFor(null)}
        />
      )}
    </div>
  );
}
