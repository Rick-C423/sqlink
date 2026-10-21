import type { EditorState } from "@/stores/editorStore";
import type { EditContext, QueryExecution, QueryResult, QueryTarget, ResolvedTable } from "@/types/database";
import { extractSingleTable, quoteTable } from "@/lib/sqlTable";
import { GRID_READ_ONLY_REASON } from "@/lib/gridEdits";
import { guardGridDraft, hasGridDraftChanges, queryDraftKey } from "@/stores/gridDraftStore";
import type { SessionCapabilities } from "@/types/task";

interface QueryService {
  executeQuery(connectionId: string, sql: string, database?: string, queryId?: number, record?: boolean, taskId?: string): Promise<QueryResult>;
  executeBatch(connectionId: string, database: string | undefined, statements: string[], queryId?: number, taskId?: string): Promise<number>;
  cancelQuery(connectionId: string, queryId: number, taskId?: string): Promise<void>;
  getSessionCapabilities?(connectionId: string): Promise<SessionCapabilities>;
  resolveTable(connectionId: string, database: string, tableParts: string[]): Promise<ResolvedTable>;
  applyGridEdits(): Promise<never>;
}

let lastTaskId = 0;
function nextTaskId(): number {
  lastTaskId = Math.max(Date.now(), lastTaskId + 1);
  return lastTaskId;
}

/** 所有后续操作只读取执行记录，避免从全局连接重新推导结果来源。 */
export function createQueryActions(store: { getState(): EditorState }, service: QueryService) {
  const tabFor = (id: string) => store.getState().tabs.find((tab) => tab.id === id);
  const dispatched = new Set<number>();
  const applyCapabilities = async (id: string, source: QueryExecution) => {
    if (!service.getSessionCapabilities) return;
    let allowed = false;
    try { allowed = (await service.getSessionCapabilities(source.connectionId)).cancellation; } catch { /* 能力未确认时保持禁用，查询本身仍可返回具体错误。 */ }
    source.canCancel = allowed;
    store.getState().updateCancellation(id, source.taskId, { canCancel: allowed });
  };

  const execute = async (id: string, sql: string, target: QueryTarget, record = true) => {
    const draftKey = queryDraftKey(id);
    if (hasGridDraftChanges(draftKey) && !await guardGridDraft(draftKey)) return;
    if (!tabFor(id)) return;
    const taskId = nextTaskId();
    const source: QueryExecution = {
      connectionId: target.connectionId, database: target.database, driverType: target.driverType,
      taskId, queryId: taskId, sql,
      backendTaskId: crypto.randomUUID(), canCancel: !["mssql", "sqlserver"].includes(target.driverType),
    };
    store.getState().beginExecution(id, source);
    try {
      if (service.getSessionCapabilities) await applyCapabilities(id, source);
      if (tabFor(id)?.execution?.taskId !== taskId) return;
      let editContext: EditContext | null = null;
      const reference = extractSingleTable(sql, source.driverType);
      if (reference) {
        try {
          // PostgreSQL 会折叠未引用名称的 ASCII 大写；显式引号中的名称必须原样保留。
          const isPostgres = source.driverType === "postgres" || source.driverType === "postgresql";
          const parts = reference.parts.map((part, index) => isPostgres && !reference.quoted[index]
            ? part.replace(/[A-Z]/g, (letter) => letter.toLowerCase()) : part);
          const resolved = await service.resolveTable(source.connectionId, source.database, parts);
          editContext = {
            connectionId: source.connectionId,
            database: resolved.database,
            schema: resolved.schema,
            table: resolved.table,
            driverType: source.driverType,
            primaryKeys: resolved.columns.filter((column) => column.isPrimaryKey).map((column) => column.name),
          };
          source.refreshSql = sql.slice(0, reference.start) + quoteTable(editContext) + sql.slice(reference.end);
        } catch {
          // 无法确定真实表身份时保留查询结果，只关闭结果编辑能力。
        }
      }
      if (tabFor(id)?.execution?.taskId !== taskId) return;
      // 查询和编辑使用同一份已解析的表身份，避免两次请求之间 search_path 等状态变化。
      dispatched.add(taskId);
      const result = await service.executeQuery(
        source.connectionId, source.refreshSql ?? sql, source.database, source.queryId, record, source.backendTaskId
      );
      if (result.task && (result.task.protocolVersion !== 1 || result.task.taskId !== source.backendTaskId ||
        result.task.connectionId !== source.connectionId || result.task.queryId !== source.queryId || result.task.phase !== "succeeded")) {
        throw new Error("任务回执与固定执行来源不一致，请核查执行状态。");
      }
      // SQL 的显示列名可能是表达式或别名；解析到单表也不授予网格写权限。
      store.getState().completeExecution(id, source, result, null);
    } catch (error) {
      const applied = store.getState().completeExecution(id, source, {
        columns: [], rows: [], executionTimeMs: 0, error: String(error),
      }, null);
      if (applied) throw error;
    } finally {
      dispatched.delete(taskId);
    }
  };

  const executeAll = async (id: string, statements: string[], target: QueryTarget) => {
    const draftKey = queryDraftKey(id);
    if (hasGridDraftChanges(draftKey) && !await guardGridDraft(draftKey)) return;
    if (!tabFor(id)) return;
    const taskId = nextTaskId();
    const source: QueryExecution = {
      connectionId: target.connectionId, database: target.database, driverType: target.driverType,
      taskId, queryId: taskId, backendTaskId: crypto.randomUUID(),
      canCancel: !["mssql", "sqlserver"].includes(target.driverType), sql: statements.join("\n"),
    };
    store.getState().beginExecution(id, source);
    const started = Date.now();
    try {
      if (service.getSessionCapabilities) await applyCapabilities(id, source);
      if (tabFor(id)?.execution?.taskId !== taskId) return;
      dispatched.add(taskId);
      const rowsAffected = await service.executeBatch(source.connectionId, source.database, statements, source.queryId, source.backendTaskId);
      store.getState().completeExecution(id, source, {
        columns: [], rows: [], rowsAffected, executionTimeMs: Date.now() - started,
      }, null);
    } catch (error) {
      const applied = store.getState().completeExecution(id, source, {
        columns: [], rows: [], executionTimeMs: Date.now() - started, error: String(error),
      }, null);
      if (applied) throw error;
    } finally {
      dispatched.delete(taskId);
    }
  };

  return {
    execute,
    executeAll,
    async cancel(id: string) {
      const execution = tabFor(id)?.execution;
      if (execution?.queryId !== undefined) {
        if (execution.canCancel === false) throw new Error("该驱动尚不支持取消，任务可能仍在服务器执行。");
        if (tabFor(id)?.cancelRequested) return;
        if (dispatched.has(execution.taskId)) {
          store.getState().updateCancellation(id, execution.taskId, { cancelRequested: true });
          try { await service.cancelQuery(execution.connectionId, execution.queryId, execution.backendTaskId); }
          catch (error) { store.getState().updateCancellation(id, execution.taskId, { cancelRequested: false }); throw error; }
        } else {
          // 表身份仍在解析时尚未下发用户 SQL，取消后不再启动该查询。
          store.getState().completeExecution(id, execution, {
            columns: [], rows: [], executionTimeMs: 0, error: "查询已取消",
          }, null);
        }
      }
    },
    async applyEdits(id: string, sourceTaskId: number | undefined, statements: string[]) {
      const tab = tabFor(id);
      const source = tab?.resultSource;
      if (!source || source.taskId !== sourceTaskId || tab?.isExecuting) {
        throw new Error("结果已更新或来源不可用，请基于当前结果重新编辑");
      }
      // 不把旧前端生成的 SQL 交给通用批执行；后端也独立拒绝缺少可信上下文的网格写入。
      void statements;
      await service.applyGridEdits();
      throw new Error(GRID_READ_ONLY_REASON);
    },
  };
}
