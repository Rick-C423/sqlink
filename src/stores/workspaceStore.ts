import { create } from "zustand";
import { useEditorStore } from "@/stores/editorStore";
import { t } from "@/i18n";

export type WorkspaceTabKind = "query" | "database" | "table" | "view" | "createTable" | "users" | "server";

export interface WorkspaceTab {
  id: string;
  kind: WorkspaceTabKind;
  title: string;
  connectionId: string;
  database?: string;
  table?: string;
  /** kind === "query" 时对应的 editorStore tab id */
  editorTabId?: string;
}

interface WorkspaceState {
  tabs: WorkspaceTab[];
  activeId: string | null;

  /** 打开/聚焦数据库概览 tab */
  openDatabase: (connectionId: string, database: string) => void;
  /** 打开/聚焦表视图 tab */
  openTable: (connectionId: string, database: string, table: string) => void;
  /** 打开/聚焦视图 tab（数据只读 + 定义） */
  openView: (connectionId: string, database: string, view: string) => void;
  /** 打开/聚焦新建表设计器 tab（每个库一个） */
  openCreateTable: (connectionId: string, database: string) => void;
  /** 打开/聚焦用户与权限管理 tab（每个连接一个） */
  openUsers: (connectionId: string) => void;
  /** 打开/聚焦服务器概览 tab（每个连接一个），name 为连接名 */
  openServer: (connectionId: string, name: string) => void;
  /** 新建查询 tab（始终排在最后），可预填 SQL */
  openQuery: (connectionId: string, content?: string) => void;
  activate: (id: string) => void;
  close: (id: string) => void;
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
    const id = `db::${connectionId}::${database}`;
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

  openTable: (connectionId, database, table) => {
    const id = `table::${connectionId}::${database}::${table}`;
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "table",
        title: table,
        connectionId,
        database,
        table,
      };
      const idx = insertIndexFor(state.tabs, "table");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openView: (connectionId, database, view) => {
    const id = `view::${connectionId}::${database}::${view}`;
    const existing = get().tabs.find((t) => t.id === id);
    if (existing) {
      set({ activeId: id });
      return;
    }
    set((state) => {
      const tab: WorkspaceTab = {
        id,
        kind: "view",
        title: view,
        connectionId,
        database,
        table: view,
      };
      const idx = insertIndexFor(state.tabs, "view");
      const tabs = [...state.tabs];
      tabs.splice(idx, 0, tab);
      return { tabs, activeId: id };
    });
  },

  openCreateTable: (connectionId, database) => {
    const id = `createTable::${connectionId}::${database}`;
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

  close: (id) =>
    set((state) => {
      const idx = state.tabs.findIndex((t) => t.id === id);
      const closing = state.tabs[idx];
      const tabs = state.tabs.filter((t) => t.id !== id);

      // 查询 tab 关闭时同步关闭编辑器 tab
      if (closing?.kind === "query" && closing.editorTabId) {
        useEditorStore.getState().closeTab(closing.editorTabId);
      }

      const activeId =
        state.activeId === id
          ? tabs[Math.min(idx, tabs.length - 1)]?.id ?? null
          : state.activeId;
      return { tabs, activeId };
    }),
}));
