import { create } from "zustand";
import type { EditContext, QueryResult } from "@/types/database";
import { tauriService } from "@/services/tauriService";
import type { ChangeSetRequest } from "@/types/typedValue";
import type { CommitReceipt } from "@/types/commitReceipt";
import type { ResultContext } from "@/types/resultContext";
import { validateCommitReceipt } from "@/lib/commitReceipt";
import { cloneCellInput } from "@/lib/typedValue";
import { typedValueInputText, type GridCellIntent } from "@/lib/gridCellInput";

export const queryDraftKey = (editorTabId: string): string => JSON.stringify(["query", editorTabId]);
export const tableDraftKey = (connectionId: string, database: string, table: string, schema?: string): string =>
  JSON.stringify(schema === undefined ? ["table", connectionId, database, table] : ["table", connectionId, database, schema, table]);

export interface GridEditingCell {
  key: string;
  value: string;
  intent?: GridCellIntent;
}

export interface GridDraft {
  sourceResult?: QueryResult;
  sourceContext?: EditContext | null;
  edits: Record<string, string>;
  cellIntents?: Record<string, GridCellIntent>;
  typedSave?: (draft: GridDraft) => Promise<boolean>;
  newRows: Record<number, string>[];
  deletedRows: Set<number>;
  editingCell: GridEditingCell | null;
  submitting: boolean;
  error: string | null;
  save?: (draft: GridDraft) => Promise<void>;
  saveState?: "submitting" | "committed" | "committedRefreshFailed" | "conflict" | "rolledBack" | "failedBeforeCommit" | "unknown";
  commitReceipt?: CommitReceipt;
  pendingRequest?: ChangeSetRequest;
  pendingContext?: Pick<ResultContext, "connectionId" | "connectionGeneration">;
  refreshAfterCommit?: () => Promise<void>;
  submissionToken?: number;
}

/** 把尚未失焦的输入纳入草稿；这里只生成快照，不触发提交。 */
export function materializeGridDraft(draft: GridDraft): GridDraft {
  const cell = draft.editingCell;
  if (!cell) return draft;
  const edits = { ...draft.edits };
  const cellIntents = { ...draft.cellIntents };
  const newRows = draft.newRows.map((row) => ({ ...row }));
  const intent = cell.intent ?? "value";
  if (cell.key.startsWith("new:")) {
    const [, row, column] = cell.key.split(":").map(Number);
    if (newRows[row]) {
      if (intent === "default" || (!draft.sourceResult?.resultContext && cell.value === "")) {
        delete newRows[row][column]; delete cellIntents[cell.key];
      } else { newRows[row][column] = cell.value; cellIntents[cell.key] = intent; }
    }
  } else {
    const [row, column] = cell.key.split(":").map(Number);
    const typed = draft.sourceResult?.typedRows?.[row]?.[column];
    const value = draft.sourceResult?.rows[row]?.values[column];
    const original = draft.sourceResult?.resultContext ? typedValueInputText(typed) : value == null ? "NULL" : String(value);
    const originalIntent = typed?.kind === "null" ? "null" : "value";
    if (cell.value === original && intent === originalIntent) { delete edits[cell.key]; delete cellIntents[cell.key]; }
    else { edits[cell.key] = cell.value; cellIntents[cell.key] = intent; }
  }
  return { ...draft, edits, cellIntents, newRows, editingCell: null };
}

export function isGridDraftDirty(draft: GridDraft | undefined): boolean {
  if (!draft) return false;
  const current = materializeGridDraft(draft);
  return Object.keys(current.edits).length > 0 || current.newRows.length > 0 || current.deletedRows.size > 0;
}

function emptyDraft(sourceResult?: QueryResult, sourceContext?: EditContext | null, save?: GridDraft["save"]): GridDraft {
  return { sourceResult, sourceContext, save, edits: {}, newRows: [], deletedRows: new Set(), editingCell: null, submitting: false, error: null };
}

export type GridNavigationChoice = "save" | "discard" | "cancel";
export interface GridNavigationRequest { keys: string[]; saving: boolean }

interface GridDraftState {
  drafts: Record<string, GridDraft>;
  navigation: GridNavigationRequest | null;
  ensureDraft: (key: string, result?: QueryResult, context?: EditContext | null, save?: GridDraft["save"], typedSave?: GridDraft["typedSave"], refresh?: GridDraft["refreshAfterCommit"]) => void;
  updateDraft: (key: string, update: (draft: GridDraft) => Partial<GridDraft>) => void;
  saveDraft: (key: string) => Promise<boolean>;
  saveChangeSetDraft: (key: string, request: ChangeSetRequest, refresh: () => Promise<void>, taskId?: string) => Promise<boolean>;
  checkChangeSetReceipt: (key: string) => Promise<boolean>;
  refreshCommittedDraft: (key: string) => Promise<boolean>;
  discardDraft: (key: string) => void;
  chooseNavigation: (choice: GridNavigationChoice) => Promise<void>;
}

let finishNavigation: ((allowed: boolean) => void) | null = null;
let nextSubmissionToken = 0;

function hasUnresolvedSave(draft: GridDraft | undefined): boolean {
  return !!draft && (draft.saveState === "unknown" || draft.saveState === "committedRefreshFailed");
}

/** 仅匹配此次提交的草稿，迟到响应不能修改后来建立的草稿或查询结果。 */
function matchingSubmission(key: string, token: number): GridDraft | undefined {
  const draft = useGridDraftStore.getState().drafts[key];
  return draft?.submissionToken === token ? draft : undefined;
}

function patchSubmission(key: string, token: number, patch: Partial<GridDraft>): boolean {
  const draft = matchingSubmission(key, token);
  if (!draft) return false;
  useGridDraftStore.setState((state) => ({ drafts: { ...state.drafts, [key]: { ...draft, ...patch } } }));
  return true;
}

async function refreshSubmission(key: string, token: number): Promise<boolean> {
  const draft = matchingSubmission(key, token);
  if (draft?.commitReceipt?.status !== "committed" || !draft.refreshAfterCommit) return false;
  if (!patchSubmission(key, token, { submitting: true, error: null })) return false;
  try {
    await draft.refreshAfterCommit();
    return patchSubmission(key, token, { submitting: false, saveState: "committed", error: null });
  } catch (error) {
    patchSubmission(key, token, { submitting: false, saveState: "committedRefreshFailed", error: `数据已提交，刷新失败，请重试读取。${String(error)}` });
    return false;
  }
}

async function receiveReceipt(key: string, token: number, value: unknown): Promise<boolean> {
  const draft = matchingSubmission(key, token);
  if (!draft?.pendingRequest) return false;
  const receipt = validateCommitReceipt(value, draft.pendingRequest, draft.pendingContext);
  if (receipt.status !== "committed") {
    const messages = { unknown: "提交结果未知，请核查回执，禁止重复提交。", conflict: "保存发生冲突，草稿已保留。",
      rolledBack: "保存已回滚，草稿已保留。", failedBeforeCommit: "提交前失败，草稿已保留。" };
    patchSubmission(key, token, { commitReceipt: receipt, saveState: receipt.status, submitting: false,
      error: messages[receipt.status] });
    return false;
  }
  // 已提交输入先清除。刷新失败只保留回执与读取重试入口，不恢复已执行的输入。
  // 清草稿到刷新完成之间仍独占保存，避免同步订阅在两步之间释放或替换结果。
  if (!patchSubmission(key, token, { edits: {}, cellIntents: {}, newRows: [], deletedRows: new Set(), editingCell: null,
    commitReceipt: receipt, saveState: "committed", submitting: true, error: null })) return false;
  return refreshSubmission(key, token);
}

/** 草稿不再引用旧结果时回收后端资源；关闭/过期后的重复释放由后端幂等处理。 */
function releaseUnreferencedResult(result?: QueryResult): void {
  const context = result?.resultContext;
  if (!context) return;
  const referenced = Object.values(useGridDraftStore.getState().drafts).some((draft) => {
    const other = draft.sourceResult?.resultContext;
    return other?.resultId === context.resultId && other.revision === context.revision;
  });
  if (!referenced) void tauriService.releaseResultContext(context.resultId, context.revision).catch(() => {});
}

export const useGridDraftStore = create<GridDraftState>((set, get) => ({
  drafts: {},
  navigation: null,
  ensureDraft: (key, result, context, save, typedSave, refresh) => {
    const existing = get().drafts[key];
    // 脏草稿始终绑定第一次编辑的结果和来源，不能迁移到后来返回的行。
    if (existing && (isGridDraftDirty(existing) || existing.submitting || hasUnresolvedSave(existing))) {
      // 组件重挂时仅重绑受控保存/读取回调，原始结果和未决请求身份仍固定。
      if (!existing.submitting && typedSave && (existing.typedSave !== typedSave ||
        (existing.pendingRequest && refresh && existing.refreshAfterCommit !== refresh))) {
        set((state) => ({ drafts: { ...state.drafts, [key]: { ...existing, typedSave,
          refreshAfterCommit: existing.pendingRequest ? refresh ?? existing.refreshAfterCommit : existing.refreshAfterCommit } } }));
      }
      return;
    }
    if (existing?.sourceResult === result && existing?.sourceContext === context && existing?.save === save && existing?.typedSave === typedSave) return;
    if (existing && existing.sourceResult === result && existing.sourceContext === context) {
      set((state) => ({ drafts: { ...state.drafts, [key]: { ...existing, save, typedSave } } }));
      return;
    }
    set((state) => ({ drafts: { ...state.drafts, [key]: { ...emptyDraft(result, context, save), typedSave } } }));
    releaseUnreferencedResult(existing?.sourceResult);
  },
  updateDraft: (key, update) => {
    const draft = get().drafts[key];
    if (!draft || draft.submitting || hasUnresolvedSave(draft) || get().navigation?.saving) return;
    set((state) => ({ drafts: { ...state.drafts, [key]: { ...draft, ...update(draft) } } }));
  },
  saveDraft: async (key) => {
    const draft = get().drafts[key];
    if (draft?.submitting) return false;
    if (draft?.saveState === "committedRefreshFailed") return get().refreshCommittedDraft(key);
    if (draft?.saveState === "unknown") return get().checkChangeSetReceipt(key);
    if (draft?.pendingRequest && !draft.typedSave && isGridDraftDirty(draft)) return false;
    if (!draft || !isGridDraftDirty(draft)) return true;
    if (draft.submitting) return false;
    const current = materializeGridDraft(draft);
    if (current.typedSave) {
      try { return await current.typedSave(current); }
      catch (error) {
        const latest = get().drafts[key];
        if (latest && !latest.submitting) set((state) => ({ drafts: { ...state.drafts, [key]: { ...latest, error: String(error) } } }));
        return false;
      }
    }
    const submitting = { ...current, submitting: true, error: null };
    set((state) => ({ drafts: { ...state.drafts, [key]: submitting } }));
    try {
      if (!current.save) throw new Error("当前草稿没有可用的保存操作。");
      await current.save(current);
      if (get().drafts[key] !== submitting) return false;
      set((state) => ({ drafts: { ...state.drafts, [key]: emptyDraft(current.sourceResult, current.sourceContext, current.save) } }));
      return true;
    } catch (error) {
      if (get().drafts[key] !== submitting) return false;
      set((state) => ({ drafts: { ...state.drafts, [key]: { ...current, submitting: false, error: String(error) } } }));
      return false;
    }
  },
  saveChangeSetDraft: async (key, request, refresh, taskId) => {
    const draft = get().drafts[key];
    if (!draft || draft.submitting || hasUnresolvedSave(draft) || draft.commitReceipt?.status === "committed") return false;
    // 即使调用方保留旧请求，也不得重写已经提交过的同一变更集。
    if (draft.pendingRequest?.changeSetId === request.changeSetId) return false;
    const context = draft.sourceResult?.resultContext;
    const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
    let snapshot: ChangeSetRequest;
    try {
      if (!context || request.protocolVersion !== 1 || context.resultId !== request.resultId || context.revision !== request.revision ||
        !uuid.test(request.changeSetId) || !uuid.test(request.resultId) || !Number.isSafeInteger(request.revision) || request.revision < 1 ||
        !request.operations.length || request.operations.some((operation) => !uuid.test(operation.opId)) ||
        new Set(request.operations.map((operation) => operation.opId.toLowerCase())).size !== request.operations.length) {
        throw new Error("提交必须使用草稿固定的原始结果身份与唯一操作身份。");
      }
      snapshot = { ...request, operations: request.operations.map((operation) => operation.kind === "delete" ? { ...operation } :
        { ...operation, changes: Object.fromEntries(Object.entries(operation.changes).map(([column, input]) => [column, cloneCellInput(input)])) }) };
    } catch (error) {
      set((state) => ({ drafts: { ...state.drafts, [key]: { ...draft, error: String(error) } } }));
      return false;
    }
    const token = ++nextSubmissionToken;
    set((state) => ({ drafts: { ...state.drafts, [key]: { ...materializeGridDraft(draft), pendingRequest: snapshot,
      pendingContext: { connectionId: context.connectionId, connectionGeneration: context.connectionGeneration },
      refreshAfterCommit: refresh, submissionToken: token, commitReceipt: undefined, saveState: "submitting", submitting: true, error: null } } }));
    try {
      return await receiveReceipt(key, token, await tauriService.commitChangeSet(snapshot, taskId));
    } catch (error) {
      patchSubmission(key, token, { submitting: false, saveState: "unknown", error: `提交结果未知，请核查回执，禁止重复提交。${String(error)}` });
      return false;
    }
  },
  checkChangeSetReceipt: async (key) => {
    const draft = get().drafts[key];
    if (!draft || draft.submitting || draft.saveState !== "unknown" || !draft.pendingRequest || !draft.submissionToken) return false;
    const { pendingRequest: request, submissionToken: token } = draft;
    patchSubmission(key, token, { submitting: true, error: null });
    try {
      return await receiveReceipt(key, token, await tauriService.getChangeSetReceipt(request.changeSetId, request.resultId, request.revision));
    } catch (error) {
      patchSubmission(key, token, { submitting: false, saveState: "unknown", error: `回执核查失败，仍需核查，禁止重复提交。${String(error)}` });
      return false;
    }
  },
  refreshCommittedDraft: async (key) => {
    const draft = get().drafts[key];
    if (!draft || draft.submitting || draft.commitReceipt?.status !== "committed" || !draft.submissionToken || !draft.refreshAfterCommit) return false;
    return refreshSubmission(key, draft.submissionToken);
  },
  discardDraft: (key) => {
    const draft = get().drafts[key];
    if (!draft || draft.submitting || hasUnresolvedSave(draft)) return;
    set((state) => ({ drafts: { ...state.drafts, [key]: { ...emptyDraft(draft.sourceResult, draft.sourceContext, draft.save), typedSave: draft.typedSave } } }));
  },
  chooseNavigation: async (choice) => {
    const request = get().navigation;
    if (!request || request.saving) return;
    let allowed = false;
    if (choice === "discard") {
      for (const key of request.keys) get().discardDraft(key);
      allowed = request.keys.every((key) => !hasGridDraftChanges(key));
    } else if (choice === "save") {
      set({ navigation: { ...request, saving: true } });
      allowed = true;
      for (const key of request.keys) {
        if (!await get().saveDraft(key)) { allowed = false; break; }
      }
    }
    const resolve = finishNavigation;
    finishNavigation = null;
    set({ navigation: null });
    resolve?.(allowed);
  },
}));

export function guardGridDraft(key: string): Promise<boolean> {
  return guardGridDrafts([key]);
}

export function hasGridDraftChanges(key: string): boolean {
  const draft = useGridDraftStore.getState().drafts[key];
  return !!draft?.submitting || hasUnresolvedSave(draft) || isGridDraftDirty(draft);
}

/** 页面关闭后只释放干净草稿，防止意外调用丢失用户输入。 */
export function releaseGridDraft(key: string): void {
  const state = useGridDraftStore.getState();
  if (hasGridDraftChanges(key) || state.navigation?.keys.includes(key)) return;
  const previous = state.drafts[key]?.sourceResult;
  const drafts = { ...state.drafts };
  delete drafts[key];
  useGridDraftStore.setState({ drafts });
  releaseUnreferencedResult(previous);
}

/** 对话框与保存独占导航，避免并发操作绕过同一份草稿。 */
export function guardGridDrafts(keys: string[]): Promise<boolean> {
  const state = useGridDraftStore.getState();
  if (state.navigation || Object.values(state.drafts).some((draft) => draft.submitting)) return Promise.resolve(false);
  const dirtyKeys = [...new Set(keys)].filter((key) => hasGridDraftChanges(key));
  if (!dirtyKeys.length) return Promise.resolve(true);
  return new Promise((resolve) => {
    finishNavigation = resolve;
    useGridDraftStore.setState({ navigation: { keys: dirtyKeys, saving: false } });
  });
}
