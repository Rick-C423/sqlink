import { invoke } from "@tauri-apps/api/core";
import { ConnectionConfig, QueryResult, TableInfo, ColumnInfo, IndexInfo, ForeignKeyInfo, ImportSummary, DumpSummary } from "@/types/database";

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
    queryId?: number
  ): Promise<QueryResult> {
    return await invoke("query_execute", { connectionId, sql, database, queryId });
  },

  async cancelQuery(connectionId: string, queryId: number): Promise<void> {
    await invoke("query_cancel", { connectionId, queryId });
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

  async getTables(connectionId: string, database?: string): Promise<TableInfo[]> {
    return await invoke("schema_get_tables", { connectionId, database });
  },

  async getColumns(connectionId: string, database: string | undefined, table: string): Promise<ColumnInfo[]> {
    return await invoke("schema_get_columns", { connectionId, database, table });
  },

  async getIndexes(connectionId: string, database: string | undefined, table: string): Promise<IndexInfo[]> {
    return await invoke("schema_get_indexes", { connectionId, database, table });
  },

  async getForeignKeys(connectionId: string, database: string | undefined, table: string): Promise<ForeignKeyInfo[]> {
    return await invoke("schema_get_foreign_keys", { connectionId, database, table });
  },
};
