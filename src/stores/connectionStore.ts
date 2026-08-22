import { create } from "zustand";
import { ConnectionConfig, TreeNode } from "@/types/database";

interface ConnectionState {
  /** 已保存的连接配置（密码为空掩码） */
  saved: ConnectionConfig[];
  /** 当前已建立活动连接的 id 集合 */
  liveIds: Set<string>;
  activeConnection: string | null;
  selectedDatabase: string | null;
  treeData: TreeNode[];
  expandedNodes: Set<string>;
  loadingNodes: Set<string>;
  /** 是否在连接树中显示系统数据库（默认隐藏） */
  showSystemDatabases: boolean;

  setSaved: (list: ConnectionConfig[]) => void;
  upsertSaved: (config: ConnectionConfig) => void;
  removeSaved: (id: string) => void;
  markLive: (id: string, live: boolean) => void;
  setActiveConnection: (id: string | null) => void;
  setSelectedDatabase: (db: string | null) => void;
  expandNode: (id: string) => void;
  collapseNode: (id: string) => void;
  setNodeLoading: (id: string, loading: boolean) => void;
  updateNodeChildren: (id: string, children: TreeNode[]) => void;
  /** 清空节点子树（刷新用） */
  clearNodeChildren: (id: string) => void;
  toggleShowSystemDatabases: () => void;
}

/** 展平树，取已有节点（用于重建时保留已加载的子节点） */
function flatten(nodes: TreeNode[], into: Map<string, TreeNode>) {
  for (const n of nodes) {
    into.set(n.id, n);
    if (n.children) flatten(n.children, into);
  }
}

/** 从 saved + liveIds 重建连接树（扁平连接列表），保留已有子树 */
function buildTree(
  saved: ConnectionConfig[],
  liveIds: Set<string>,
  prevTree: TreeNode[]
): TreeNode[] {
  const prev = new Map<string, TreeNode>();
  flatten(prevTree, prev);

  return saved.map((cfg) => {
    const id = cfg.id!;
    const old = prev.get(id);
    return {
      id,
      type: "connection" as const,
      name: cfg.name,
      children: old?.children,
      metadata: {
        color: cfg.color,
        connected: liveIds.has(id),
        driverType: cfg.driverType,
      },
    };
  });
}

export const useConnectionStore = create<ConnectionState>((set) => ({
  saved: [],
  liveIds: new Set(),
  activeConnection: null,
  selectedDatabase: null,
  treeData: [],
  expandedNodes: new Set(),
  loadingNodes: new Set(),
  showSystemDatabases: false,

  toggleShowSystemDatabases: () =>
    set((state) => ({ showSystemDatabases: !state.showSystemDatabases })),

  setSaved: (list) =>
    set((state) => ({
      saved: list,
      treeData: buildTree(list, state.liveIds, state.treeData),
    })),

  upsertSaved: (config) =>
    set((state) => {
      const idx = state.saved.findIndex((c) => c.id === config.id);
      const saved =
        idx === -1
          ? [...state.saved, config]
          : state.saved.map((c, i) => (i === idx ? config : c));
      return { saved, treeData: buildTree(saved, state.liveIds, state.treeData) };
    }),

  removeSaved: (id) =>
    set((state) => {
      const saved = state.saved.filter((c) => c.id !== id);
      const liveIds = new Set(state.liveIds);
      liveIds.delete(id);
      return {
        saved,
        liveIds,
        treeData: buildTree(saved, liveIds, state.treeData),
        activeConnection: state.activeConnection === id ? null : state.activeConnection,
        selectedDatabase: state.activeConnection === id ? null : state.selectedDatabase,
      };
    }),

  markLive: (id, live) =>
    set((state) => {
      const liveIds = new Set(state.liveIds);
      if (live) liveIds.add(id);
      else liveIds.delete(id);
      // 断开时清掉该连接的子树，强制下次重新加载
      const pruned = state.treeData.map((n) =>
        n.id === id ? { ...n, children: undefined } : n
      );
      return {
        liveIds,
        treeData: buildTree(state.saved, liveIds, pruned),
        selectedDatabase: state.activeConnection === id && !live ? null : state.selectedDatabase,
        activeConnection: state.activeConnection === id && !live ? null : state.activeConnection,
      };
    }),

  setActiveConnection: (id) => set({ activeConnection: id, selectedDatabase: null }),

  setSelectedDatabase: (db) => set({ selectedDatabase: db }),

  expandNode: (id) =>
    set((state) => {
      const newSet = new Set(state.expandedNodes);
      newSet.add(id);
      return { expandedNodes: newSet };
    }),

  collapseNode: (id) =>
    set((state) => {
      const newSet = new Set(state.expandedNodes);
      newSet.delete(id);
      return { expandedNodes: newSet };
    }),

  setNodeLoading: (id, loading) =>
    set((state) => {
      const newSet = new Set(state.loadingNodes);
      if (loading) newSet.add(id);
      else newSet.delete(id);
      return { loadingNodes: newSet };
    }),

  updateNodeChildren: (id, children) =>
    set((state) => {
      const updateNode = (nodes: TreeNode[]): TreeNode[] => {
        return nodes.map((node) => {
          if (node.id === id) {
            return { ...node, children, isExpanded: true, isLoading: false };
          }
          if (node.children) {
            return { ...node, children: updateNode(node.children) };
          }
          return node;
        });
      };
      return { treeData: updateNode(state.treeData) };
    }),

  clearNodeChildren: (id) =>
    set((state) => {
      const clear = (nodes: TreeNode[]): TreeNode[] => {
        return nodes.map((node) => {
          if (node.id === id) {
            return { ...node, children: undefined };
          }
          if (node.children) {
            return { ...node, children: clear(node.children) };
          }
          return node;
        });
      };
      return { treeData: clear(state.treeData) };
    }),
}));
