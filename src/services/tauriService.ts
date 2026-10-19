import { invoke } from "@tauri-apps/api/core";
import { ConnectionConfig, QueryResult, TableInfo, DatabaseInfo, ColumnInfo, IndexInfo, ForeignKeyInfo, ImportSummary, DumpSummary, HistoryEntry, DbUserInfo, CurrentUserInfo, McpSettingsSnapshot, McpSettingsInput, ResolvedTable } from "@/types/database";

export const tauriService = {
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
    path: string
  ): Promise<number> {
    return await invoke("table_export_csv", { connectionId, database, table, path });
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
    return await invoke("sql_import_file", { connectionId, database, path, onConflict });
  },

  /** 转储 SQL 文件：tables 为空导出整个库，返回导出汇总 */
  async dumpSqlFile(
    connectionId: string,
    database: string,
    tables: string[] | undefined,
    path: string,
    includeDrop: boolean,
    includeData: boolean
  ): Promise<DumpSummary> {
    return await invoke("sql_dump_file", {
      connectionId,
      database,
      tables,
      path,
      includeDrop,
      includeData,
    });
  },

  // 查询执行
  async executeQuery(
    connectionId: string,
    sql: string,
    database?: string,
    queryId?: number,
    record?: boolean
  ): Promise<QueryResult> {
    return await invoke("query_execute", { connectionId, sql, database, queryId, record });
  },

  async cancelQuery(connectionId: string, queryId: number): Promise<void> {
    await invoke("query_cancel", { connectionId, queryId });
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

  /** 事务批量执行写语句（数据网格编辑提交），返回总影响行数 */
  async executeBatch(
    connectionId: string,
    database: string | undefined,
    statements: string[]
  ): Promise<number> {
    return await invoke("query_execute_batch", { connectionId, database, statements });
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

  async getViewDefinition(connectionId: string, database: string | undefined, view: string): Promise<string> {
    return await invoke("schema_get_view_definition", { connectionId, database, view });
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
