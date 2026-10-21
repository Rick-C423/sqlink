import { invoke } from "@tauri-apps/api/core";
import { ConnectionConfig, QueryResult, TableInfo, DatabaseInfo, ColumnInfo, IndexInfo, ForeignKeyInfo, ImportSummary, DumpSummary, HistoryEntry, DbUserInfo, CurrentUserInfo, McpSettingsSnapshot, McpSettingsInput, ResolvedTable, MetadataSnapshot, ExecuteDdlRequest, ObjectIdentity } from "@/types/database";
import { metadataCache } from "@/lib/metadataCache";
import type { ControlledTablePage, ControlledTableReadRequest, ResultContext, ResultContextValidationRequest } from "@/types/resultContext";
import type { ChangeSetRequest, ChangeSetValidationResult } from "@/types/typedValue";
import type { CommitReceipt } from "@/types/commitReceipt";
import type { CredentialHealth } from "@/types/credential";
import type { DatasetPage, DatasetReadOptions, TableCountRequest, TableCountResult } from "@/types/dataset";
import type { ImportPlan, ImportPlanRequest, ImportExecuteRequest, ImportReport } from "@/types/importJob";

export const tauriService = {
  /** 读取凭据保护状态；不返回任何秘密材料。 */
  async getCredentialHealth(): Promise<CredentialHealth> {
    return await invoke("credential_health");
  },

  // 连接管理
  async testConnection(config: ConnectionConfig): Promise<void> {
    await invoke("connection_test", { config });
  },

  async createConnection(config: ConnectionConfig): Promise<string> {
    return await invoke("connection_create", { config });
  },

  async disconnect(connectionId: string): Promise<void> {
    await invoke("connection_disconnect", { connectionId });
  },

  async listConnections(): Promise<ConnectionConfig[]> {
    return await invoke("connection_list");
  },

  // 连接持久化
  /** 保存连接（密码加密落盘）并建立活动连接，返回连接 id */
  async saveConnection(config: ConnectionConfig): Promise<string> {
    return await invoke("connection_save", { config });
  },

  /** 已保存连接列表（密码掩码为空） */
  async listSavedConnections(): Promise<ConnectionConfig[]> {
    return await invoke("saved_connections_list");
  },

  /** 用已保存配置建立活动连接（密码仅在后端解密） */
  async connectSaved(id: string): Promise<string> {
    return await invoke("connection_connect_saved", { id });
  },

  /** 删除已保存连接（同时断开活动连接） */
  async deleteSaved(id: string): Promise<void> {
    await invoke("connection_delete_saved", { id });
  },

  /** 更新已保存连接（密码留空保留原密码） */
  async updateConnection(config: ConnectionConfig): Promise<void> {
    await invoke("connection_update", { config });
  },

  /** 整表导出 CSV，返回导出行数 */
  async exportTableCsv(
    connectionId: string,
    database: string | undefined,
    table: string,
    path: string,
    taskId?: string,
    schema?: string
  ): Promise<number> {
    return await invoke("table_export_csv", { connectionId, database, table, path, taskId, schema });
  },

  /**
   * 导入 SQL 转储文件，逐条执行。
   * onConflict: "error"（默认，遇已存在表报错）| "skip"（跳过已存在的表）| "overwrite"（覆盖已存在的表）
   */
  async importSqlFile(
    connectionId: string,
    database: string | undefined,
    path: string,
    onConflict: "error" | "skip" | "overwrite"
  ): Promise<ImportSummary> {
    try {
      return await invoke("sql_import_file", { connectionId, database, path, onConflict });
    } finally {
      metadataCache.invalidateConnection(connectionId);
    }
  },

  /** 转储 SQL 文件：tables 为空导出整个库，返回导出汇总 */
  async dumpSqlFile(
    connectionId: string,
    database: string,
    tables: string[] | undefined,
    path: string,
    includeDrop: boolean,
    includeData: boolean,
    taskId?: string,
    tableSources?: ObjectIdentity[]
  ): Promise<DumpSummary> {
    return await invoke("sql_dump_file", {
      connectionId,
      database,
      tables,
      path,
      includeDrop,
      includeData,
      taskId,
      tableSources,
    });
  },

  /** 请求取消固定导出任务；请求成功不代表任务已经停止。 */
  async cancelExport(connectionId: string, taskId: string): Promise<void> {
    await invoke("export_cancel", { connectionId, taskId });
  },

  // 查询执行
  async executeQuery(
    connectionId: string,
    sql: string,
    database?: string,
    queryId?: number,
    record?: boolean,
    taskId?: string
  ): Promise<QueryResult> {
    try {
      return await invoke("query_execute", { connectionId, sql, database, queryId, record, taskId });
    } finally {
      // 显式 SQL 可能改变结构、权限或会话；失败也可能已有部分语句生效。
      metadataCache.invalidateConnection(connectionId);
    }
  },

  async cancelQuery(connectionId: string, queryId: number, taskId?: string): Promise<void> {
    await invoke("query_cancel", { connectionId, queryId, taskId });
  },

  async getTaskStatus(taskId: string): Promise<import("@/types/task").TaskSnapshot> {
    return await invoke("task_get", { taskId });
  },

  async getSessionCapabilities(connectionId: string): Promise<import("@/types/task").SessionCapabilities> {
    return await invoke("connection_session_capabilities", { connectionId });
  },

  /** 后端构造分页 SQL 并登记结果身份，前端不提交列来源或主键。 */
  async readTablePage(request: ControlledTableReadRequest, options?: DatasetReadOptions): Promise<ControlledTablePage & Partial<DatasetPage>> {
    if (options) return await invoke("table_read_dataset_page", { request: { request, ...options } });
    return await invoke("table_read_page", { request });
  },

  async countTable(request: TableCountRequest): Promise<TableCountResult> {
    return await invoke("table_count", { request });
  },

  async cancelTableTask(connectionId: string, taskId: string): Promise<void> {
    await invoke("table_task_cancel", { connectionId, taskId });
  },

  async previewImport(request: ImportPlanRequest): Promise<ImportPlan> {
    return await invoke("sql_import_preview", { request });
  },

  async executeImport(request: ImportExecuteRequest): Promise<ImportReport> {
    try {
      return await invoke("sql_import_execute", { request });
    } finally {
      // 计划来源仅后端持有；断线时也可能已有部分 DDL 生效，不能保留旧目录缓存。
      metadataCache.clear();
    }
  },

  async getImportReceipt(connectionId: string, runId: string): Promise<ImportReport> {
    return await invoke("sql_import_receipt", { connectionId, runId });
  },

  async cancelImport(connectionId: string, taskId: string): Promise<void> {
    await invoke("import_cancel", { connectionId, taskId });
  },

  async validateResultContext(request: ResultContextValidationRequest): Promise<ResultContext> {
    return await invoke("result_context_validate", { request });
  },

  /** 只预检类型化变更与后端身份，不执行写入、不返回 SQL、不授予网格编辑权限。 */
  async validateChangeSet(request: ChangeSetRequest): Promise<ChangeSetValidationResult> {
    return await invoke("changeset_validate", { request });
  },

  /** 类型化提交入口；界面仍由后端门禁保持只读，不接受拼接 SQL。 */
  async commitChangeSet(request: ChangeSetRequest, taskId?: string): Promise<CommitReceipt> {
    return await invoke("changeset_commit", { request, taskId });
  },

  /** 结果未知时只读取既有回执，不能用重复提交代替核查。 */
  async getChangeSetReceipt(changeSetId: string, resultId: string, revision: number): Promise<CommitReceipt> {
    return await invoke("changeset_receipt", { changeSetId, resultId, revision });
  },

  async releaseResultContext(resultId: string, revision: number): Promise<void> {
    await invoke("result_context_release", { request: { protocolVersion: 1, resultId, revision } });
  },

  /** 旧网格写入路径已关闭，受控提交门禁齐备前后端始终拒绝。 */
  async applyGridEdits(): Promise<never> {
    return await invoke("query_apply_grid_edits");
  },

  // 查询历史
  async listHistory(connectionId: string, limit?: number): Promise<HistoryEntry[]> {
    return await invoke("history_list", { connectionId, limit });
  },

  async deleteHistory(id: string): Promise<void> {
    await invoke("history_delete", { id });
  },

  async clearHistory(connectionId: string): Promise<void> {
    await invoke("history_clear", { connectionId });
  },

  /** 编辑器显式批执行，贯穿独占租约；网格不得调用。 */
  async executeBatch(
    connectionId: string,
    database: string | undefined,
    statements: string[],
    queryId?: number,
    taskId?: string
  ): Promise<number> {
    try {
      return await invoke("query_execute_batch", { connectionId, database, statements, queryId, taskId });
    } finally {
      metadataCache.invalidateConnection(connectionId);
    }
  },

  // Schema
  async getDatabases(connectionId: string): Promise<string[]> {
    return await invoke("schema_get_databases", { connectionId });
  },

  async getDatabaseInfos(connectionId: string): Promise<DatabaseInfo[]> {
    return await invoke("schema_get_database_infos", { connectionId });
  },

  async getTables(connectionId: string, database?: string): Promise<TableInfo[]> {
    return await invoke("schema_get_tables", { connectionId, database });
  },

  async getViews(connectionId: string, database?: string): Promise<TableInfo[]> {
    return await invoke("schema_get_views", { connectionId, database });
  },

  async getViewDefinition(connectionId: string, database: string | undefined, view: string, schema?: string): Promise<string> {
    return await invoke("schema_get_view_definition", { connectionId, database, view, schema });
  },

  async getTriggers(connectionId: string, database?: string): Promise<TableInfo[]> {
    return await invoke("schema_get_triggers", { connectionId, database });
  },

  async getTriggerDefinition(connectionId: string, database: string | undefined, trigger: string): Promise<string> {
    return await invoke("schema_get_trigger_definition", { connectionId, database, trigger });
  },

  async getProcedures(connectionId: string, database?: string): Promise<TableInfo[]> {
    return await invoke("schema_get_procedures", { connectionId, database });
  },

  async getProcedureDefinition(connectionId: string, database: string | undefined, procedure: string): Promise<string> {
    return await invoke("schema_get_procedure_definition", { connectionId, database, procedure });
  },

  async getFunctions(connectionId: string, database?: string): Promise<TableInfo[]> {
    return await invoke("schema_get_functions", { connectionId, database });
  },

  async getFunctionDefinition(connectionId: string, database: string | undefined, func: string): Promise<string> {
    return await invoke("schema_get_function_definition", { connectionId, database, function: func });
  },

  async getUsers(connectionId: string, database?: string): Promise<DbUserInfo[]> {
    return await invoke("schema_get_users", { connectionId, database });
  },

  async getCurrentUser(connectionId: string, database?: string): Promise<CurrentUserInfo> {
    return await invoke("schema_get_current_user", { connectionId, database });
  },

  async getColumns(connectionId: string, database: string | undefined, table: string): Promise<ColumnInfo[]> {
    return await invoke("schema_get_columns", { connectionId, database, table });
  },

  /** 按数据库实际名称解析规则定位单表，保留完整库名和 schema。 */
  async resolveTable(connectionId: string, database: string, tableParts: string[]): Promise<ResolvedTable> {
    return await invoke("schema_resolve_table", { connectionId, database, tableParts });
  },

  /** 一次读取固定对象的列、索引和外键，并返回后端连接代次与结构版本。 */
  async getTableMetadata(connectionId: string, database: string, tableParts: string[]): Promise<MetadataSnapshot> {
    return await invoke("schema_get_table_metadata", { connectionId, database, tableParts });
  },

  /** 结构设计器显式 DDL；后端核对原连接代次、目标和结构版本。 */
  async executeDdl(request: ExecuteDdlRequest): Promise<number> {
    try {
      return await invoke("schema_execute_ddl", { request });
    } finally {
      metadataCache.invalidateConnection(request.connectionId);
    }
  },

  async getIndexes(connectionId: string, database: string | undefined, table: string): Promise<IndexInfo[]> {
    return await invoke("schema_get_indexes", { connectionId, database, table });
  },

  async getForeignKeys(connectionId: string, database: string | undefined, table: string): Promise<ForeignKeyInfo[]> {
    return await invoke("schema_get_foreign_keys", { connectionId, database, table });
  },

  /** 在系统文件管理器中打开日志目录 */
  async openLogFolder(): Promise<void> {
    await invoke("open_log_folder");
  },

  // MCP 设置
  /** 读取 MCP 设置与服务运行状态 */
  async getMcpSettings(): Promise<McpSettingsSnapshot> {
    return await invoke("mcp_settings_get");
  },

  /** 保存 MCP 设置并热应用（停用→停止服务；端口变更→自动重启；工具开关即时生效） */
  async saveMcpSettings(settings: McpSettingsInput): Promise<McpSettingsSnapshot> {
    return await invoke("mcp_settings_save", { newSettings: settings });
  },

  /** 更新已保存连接的 MCP 访问开关（写连接配置，不断开活动连接） */
  async setConnectionMcpAllowed(connectionId: string, allowed: boolean): Promise<void> {
    return await invoke("mcp_set_connection_allowed", { connectionId, allowed });
  },
};
