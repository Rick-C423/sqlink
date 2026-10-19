import type { EditorState } from "@/stores/editorStore";
import type { EditContext, QueryExecution, QueryResult, QueryTarget, ResolvedTable } from "@/types/database";
import { extractSingleTable, quoteTable } from "@/lib/sqlTable";

interface QueryService {
  executeQuery(connectionId: string, sql: string, database?: string, queryId?: number, record?: boolean): Promise<QueryResult>;
  executeBatch(connectionId: string, database: string | undefined, statements: string[]): Promise<number>;
  cancelQuery(connectionId: string, queryId: number): Promise<void>;
  resolveTable(connectionId: string, database: string, tableParts: string[]): Promise<ResolvedTable>;
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

  const execute = async (id: string, sql: string, target: QueryTarget, record = true) => {
    const taskId = nextTaskId();
    const source: QueryExecution = {
      connectionId: target.connectionId, database: target.database, driverType: target.driverType,
      taskId, queryId: taskId, sql,
    };
    store.getState().beginExecution(id, source);
    try {
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
        source.connectionId, source.refreshSql ?? sql, source.database, source.queryId, record
      );
      if (result.error || result.columns.length === 0) editContext = null;
      store.getState().completeExecution(id, source, result, editContext);
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
    const source: QueryExecution = {
      connectionId: target.connectionId, database: target.database, driverType: target.driverType,
      taskId: nextTaskId(), sql: statements.join("\n"),
    };
    store.getState().beginExecution(id, source);
    const started = Date.now();
    try {
      const rowsAffected = await service.executeBatch(source.connectionId, source.database, statements);
      store.getState().completeExecution(id, source, {
        columns: [], rows: [], rowsAffected, executionTimeMs: Date.now() - started,
      }, null);
    } catch (error) {
      const applied = store.getState().completeExecution(id, source, {
        columns: [], rows: [], executionTimeMs: Date.now() - started, error: String(error),
      }, null);
      if (applied) throw error;
    }
  };

  return {
    execute,
    executeAll,
    async cancel(id: string) {
      const execution = tabFor(id)?.execution;
      if (execution?.queryId !== undefined) {
        if (dispatched.has(execution.taskId)) {
          await service.cancelQuery(execution.connectionId, execution.queryId);
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
      const context = tab?.editContext;
      if (!source || source.taskId !== sourceTaskId || !context || tab?.isExecuting) {
        throw new Error("结果已更新或来源不可用，请基于当前结果重新编辑");
      }
      await service.executeBatch(context.connectionId, context.database, statements);
      // 保存期间启动了新查询或关闭了页签时，不用旧查询覆盖新状态。
      const current = tabFor(id);
      if (current?.resultSource?.taskId === source.taskId && !current.isExecuting) {
        await execute(id, source.refreshSql ?? source.sql, source, false);
      }
    },
  };
}
