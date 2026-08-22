import { create } from "zustand";
import { EditContext, QueryResult } from "@/types/database";

interface EditorTab {
  id: string;
  title: string;
  content: string;
  result?: QueryResult;
  isExecuting: boolean;
  executionTime?: number;
  queryId?: number;
  /** 最近一次执行的 SQL（编辑提交后用于刷新） */
  lastSql?: string;
  /** 结果集的编辑上下文（单表查询才有，否则为 null） */
  editContext?: EditContext | null;
}

interface EditorState {
  tabs: EditorTab[];
  activeTab: string | null;

  addTab: (title?: string, content?: string) => string;
  closeTab: (id: string) => void;
  setActiveTab: (id: string) => void;
  updateTabContent: (id: string, content: string) => void;
  setTabResult: (id: string, result: QueryResult) => void;
  setTabExecuting: (id: string, executing: boolean) => void;
  setTabQueryId: (id: string, queryId: number | undefined) => void;
  setTabContext: (id: string, lastSql: string, editContext: EditContext | null) => void;
  getActiveTab: () => EditorTab | undefined;
}

export const useEditorStore = create<EditorState>((set, get) => ({
  tabs: [],
  activeTab: null,

  addTab: (title, content) => {
    const id = `tab_${Date.now()}`;
    const newTab: EditorTab = {
      id,
      title: title || `查询 ${get().tabs.length + 1}`,
      content: content || "",
      isExecuting: false,
    };
    set((state) => ({
      tabs: [...state.tabs, newTab],
      activeTab: id,
    }));
    return id;
  },

  closeTab: (id) =>
    set((state) => {
      const idx = state.tabs.findIndex((t) => t.id === id);
      const newTabs = state.tabs.filter((t) => t.id !== id);
      // 关闭当前 tab 时激活相邻（优先右侧）的 tab，而不是跳到最后一个
      const newActive = state.activeTab === id
        ? newTabs[Math.min(idx, newTabs.length - 1)]?.id || null
        : state.activeTab;
      return { tabs: newTabs, activeTab: newActive };
    }),

  setActiveTab: (id) => set({ activeTab: id }),

  updateTabContent: (id, content) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, content } : t
      ),
    })),

  setTabResult: (id, result) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, result, executionTime: result.executionTimeMs, queryId: undefined } : t
      ),
    })),

  setTabQueryId: (id, queryId) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, queryId } : t
      ),
    })),

  setTabContext: (id, lastSql, editContext) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, lastSql, editContext } : t
      ),
    })),

  setTabExecuting: (id, executing) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, isExecuting: executing } : t
      ),
    })),

  getActiveTab: () => {
    const { tabs, activeTab } = get();
    return tabs.find((t) => t.id === activeTab);
  },
}));
