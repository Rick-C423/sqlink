import { create } from "zustand";
import { useEditorStore } from "@/stores/editorStore";
import { t } from "@/i18n";
import { relationTabId } from "@/lib/objectIdentity";
import { guardGridDrafts, hasGridDraftChanges, queryDraftKey, tableDraftKey, releaseGridDraft } from "@/stores/gridDraftStore";

export type WorkspaceTabKind = "query" | "database" | "table" | "view" | "createTable" | "users" | "server" | "routine";

export type RoutineKind = "trigger" | "procedure" | "function";

/** 表视图子页：数据 / 结构 */
export type TableSubTab = "data" | "structure";

export interface WorkspaceTab {
  id: string;
  kind: WorkspaceTabKind;
  title: string;
  connectionId: string;
  database?: string;
  table?: string;
  schema?: string;
  /** kind === "table" 时要打开的子页（数据/结构），由打开入口指定 */
  tableSubTab?: TableSubTab;
  /** 已打开的表 tab 被再次指定子页时递增，驱动 TableView 切换子页 */
  subTabNonce?: number;
  /** kind === "routine" 时的对象类型（触发器/存储过程/函数），对象名存在 table 字段 */
  routineKind?: RoutineKind;
  /** kind === "query" 时对应的 editorStore tab id */
  editorTabId?: string;
}

interface WorkspaceState {
  tabs: WorkspaceTab[];
  activeId: string | null;

  /** 打开/聚焦数据库概览 tab */
  openDatabase: (connectionId: string, database: string) => void;
  /** 打开/聚焦表视图 tab；subTab 指定落到「数据」还是「结构」子页 */
  openTable: (connectionId: string, database: string, table: string, subTab?: TableSubTab, schema?: string) => void;
  /** 打开/聚焦视图 tab（数据只读 + 定义） */
  openView: (connectionId: string, database: string, view: string, schema?: string) => void;
  /** 打开/聚焦触发器/存储过程/函数 tab（定义只读） */
  openRoutine: (connectionId: string, database: string, routineKind: RoutineKind, name: string) => void;
  /** 打开/聚焦新建表设计器 tab（每个库一个） */
  openCreateTable: (connectionId: string, database: string) => void;
  /** 打开/聚焦用户与权限管理 tab（每个连接一个） */
  openUsers: (connectionId: string) => void;
  /** 打开/聚焦服务器概览 tab（每个连接一个），name 为连接名 */
  openServer: (connectionId: string, name: string) => void;
  /** 新建查询 tab（始终排在最后），可预填 SQL */
  openQuery: (connectionId: string, content?: string) => void;
  activate: (id: string) => void;
  close: (id: string) => Promise<boolean>;
  closeMany: (ids: string[]) => Promise<boolean>;
}

function draftKeyForTab(tab: WorkspaceTab): string | undefined {
  if (tab.editorTabId) return queryDraftKey(tab.editorTabId);
  if ((tab.kind === "table" || tab.kind === "view") && tab.database && tab.table) {
    return tableDraftKey(tab.connectionId, tab.database, tab.table, tab.schema);
  }
}

/** 对象 tab 排在查询 tab 前面：返回新对象 tab 应插入的位置 */
function insertIndexFor(tabs: WorkspaceTab[], kind: WorkspaceTabKind): number {
  if (kind === "query") return tabs.length;
  const firstQuery = tabs.findIndex((t) => t.kind === "query");
  return firstQuery === -1 ? tabs.length : firstQuery;
}

export const useWorkspaceStore = create<WorkspaceState>((set, get) => ({
  tabs: [],
  activeId: null,

  openDatabase: (connectionId, database) => {
    const id = JSON.stringify(["database", connectionId, database]);
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "database",
        title: `📁 ${database}`,
        connectionId,
        database,
      };
      const idx = insertIndexFor(state.tabs, "database");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openTable: (connectionId, database, table, subTab, schema) => {
    const id = relationTabId("table", connectionId, database, table, schema);
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      if (subTab) {
        // tab 已打开：聚焦并切换到指定子页（nonce 驱动 TableView 同步）
        set((state) => ({
          tabs: state.tabs.map((t) =>
            t.id === id
              ? { ...t, tableSubTab: subTab, subTabNonce: (t.subTabNonce ?? 0) + 1 }
              : t
          ),
          activeId: id,
        }));
      } else {
        set({ activeId: id });
      }
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "table",
        title: schema ? `${schema}.${table}` : table,
        connectionId,
        database,
        table,
        schema,
        tableSubTab: subTab ?? "data",
      };
      const idx = insertIndexFor(state.tabs, "table");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openView: (connectionId, database, view, schema) => {
    const id = relationTabId("view", connectionId, database, view, schema);
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "view",
        title: schema ? `${schema}.${view}` : view,
        connectionId,
        database,
        table: view,
        schema,
      };
      const idx = insertIndexFor(state.tabs, "view");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openRoutine: (connectionId, database, routineKind, name) => {
    const id = JSON.stringify(["routine", routineKind, connectionId, database, name]);
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "routine",
        title: name,
        connectionId,
        database,
        table: name,
        routineKind,
      };
      const idx = insertIndexFor(state.tabs, "routine");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openCreateTable: (connectionId, database) => {
    const id = JSON.stringify(["createTable", connectionId, database]);
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "createTable",
        title: t("ws.newTableTab", { database }),
        connectionId,
        database,
      };
      const idx = insertIndexFor(state.tabs, "createTable");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openUsers: (connectionId) => {
    const id = `users::${connectionId}`;
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => ({
      tabs: [
        { id, kind: "users" as const, title: "用户与权限", connectionId },
        ...state.tabs,
      ],
      activeId: id,
    }));
  },

  openServer: (connectionId, name) => {
    const id = `server::${connectionId}`;
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const idx = insertIndexFor(state.tabs, "server");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, { id, kind: "server", title: name, connectionId });
      return { tabs, activeId: id };
    });
  },

  openQuery: (connectionId, content) => {    const editorTabId = useEditorStore.getState().addTab(undefined, content);
    const id = `query::${editorTabId}`;
    set((state) => {
      const title =
        useEditorStore.getState().tabs.find((t) => t.id === editorTabId)?.title ?? t("ws.query");
      return {
        tabs: [...state.tabs, { id, kind: "query", title, connectionId, editorTabId }],
        activeId: id,
      };
    });
  },

  activate: (id) => set({ activeId: id }),

  close: async (id) => {
    const closing = get().tabs.find((tab) => tab.id === id);
    const key = closing && draftKeyForTab(closing);
    if (key && hasGridDraftChanges(key) && !await guardGridDrafts([key])) return false;
    if (closing?.editorTabId && !await useEditorStore.getState().closeTab(closing.editorTabId)) return false;
    set((state) => {
      const idx = state.tabs.findIndex((t) => t.id === id);
      const closing = state.tabs[idx];
      const tabs = state.tabs.filter((t) => t.id !== id);

      const activeId =
        state.activeId === id
          ? tabs[Math.min(idx, tabs.length - 1)]?.id ?? null
          : state.activeId;
      return { tabs, activeId };
    });
    if (key) releaseGridDraft(key);
    return true;
  },

  closeMany: async (ids) => {
    const keys = get().tabs.filter((tab) => ids.includes(tab.id)).map(draftKeyForTab)
      .filter((key): key is string => key !== undefined);
    if (!await guardGridDrafts(keys)) return false;
    for (const id of ids) {
      if (!await get().close(id)) return false;
    }
    return true;
  },
}));
