import { create } from "zustand";
import { guardGridDraft, hasGridDraftChanges, queryDraftKey, releaseGridDraft } from "@/stores/gridDraftStore";
import { EditContext, QueryExecution, QueryResult } from "@/types/database";
import { t } from "@/i18n";

export interface EditorTab {
  id: string;
  title: string;
  content: string;
  result?: QueryResult;
  isExecuting: boolean;
  cancelRequested?: boolean;
  executionTime?: number;
  execution?: Readonly<QueryExecution>;
  /** 与当前结果一同发布的来源；保存和刷新只能使用此上下文。 */
  resultSource?: Readonly<QueryExecution>;
  /** 最近一次执行的 SQL（编辑提交后用于刷新） */
  lastSql?: string;
  /** 兼容旧结果的上下文；当前 SQL 查询均为 null，不能作为写入授权。 */
  editContext?: EditContext | null;
}

export interface EditorState {
  tabs: EditorTab[];
  activeTab: string | null;

  addTab: (title?: string, content?: string) => string;
  closeTab: (id: string) => Promise<boolean>;
  setActiveTab: (id: string) => void;
  updateTabContent: (id: string, content: string) => void;
  beginExecution: (id: string, execution: QueryExecution) => void;
  updateCancellation: (id: string, taskId: number, values: { canCancel?: boolean; cancelRequested?: boolean }) => void;
  completeExecution: (
    id: string,
    source: QueryExecution,
    result: QueryResult,
    editContext: EditContext | null
  ) => boolean;
  getActiveTab: () => EditorTab | undefined;
}

export const useEditorStore = create<EditorState>((set, get) => ({
  tabs: [],
  activeTab: null,

  addTab: (title, content) => {
    const id = `tab_${Date.now()}`;
    const newTab: EditorTab = {
      id,
      title: title || t("ws.queryN", { n: get().tabs.length + 1 }),
      content: content || "",
      isExecuting: false,
    };
    set((state) => ({
      tabs: [...state.tabs, newTab],
      activeTab: id,
    }));
    return id;
  },

  closeTab: async (id) => {
    const key = queryDraftKey(id);
    if (hasGridDraftChanges(key) && !await guardGridDraft(key)) return false;
    set((state) => {
      const idx = state.tabs.findIndex((t) => t.id === id);
      const newTabs = state.tabs.filter((t) => t.id !== id);
      // 关闭当前 tab 时激活相邻（优先右侧）的 tab，而不是跳到最后一个
      const newActive = state.activeTab === id
        ? newTabs[Math.min(idx, newTabs.length - 1)]?.id || null
        : state.activeTab;
      return { tabs: newTabs, activeTab: newActive };
    });
    releaseGridDraft(key);
    return true;
  },

  setActiveTab: (id) => set({ activeTab: id }),

  updateTabContent: (id, content) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, content } : t
      ),
    })),

  beginExecution: (id, execution) =>
    set((state) => ({
      tabs: state.tabs.map((t) =>
        t.id === id ? { ...t, isExecuting: true, cancelRequested: false, execution: Object.freeze({ ...execution }) } : t
      ),
    })),

  updateCancellation: (id, taskId, values) => set((state) => ({
    tabs: state.tabs.map((tab) => tab.id === id && tab.execution?.taskId === taskId ? {
      ...tab,
      cancelRequested: values.cancelRequested ?? tab.cancelRequested,
      execution: Object.freeze({ ...tab.execution, canCancel: values.canCancel ?? tab.execution.canCancel }),
    } : tab),
  })),

  completeExecution: (id, source, result, editContext) => {
    // 已关闭页签或被后续执行替代的请求，不能覆盖较新的结果和编辑上下文。
    if (get().tabs.find((tab) => tab.id === id)?.execution?.taskId !== source.taskId) {
      return false;
    }
    set((state) => ({
      tabs: state.tabs.map((tab) => tab.id === id ? {
        ...tab,
        result,
        executionTime: result.executionTimeMs,
        resultSource: Object.freeze({ ...source }),
        lastSql: source.sql,
        editContext,
        execution: undefined,
        isExecuting: false,
        cancelRequested: false,
      } : tab),
    }));
    return true;
  },

  getActiveTab: () => {
    const { tabs, activeTab } = get();
    return tabs.find((t) => t.id === activeTab);
  },
}));
