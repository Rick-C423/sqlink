export interface ConnectionConfig {
  id?: string;
  name: string;
  driverType: 'mysql' | 'mariadb' | 'postgres' | 'postgresql' | 'sqlite' | 'sqlserver' | 'mssql' | (string & {});
  host: string;
  port: number;
  user: string;
  password: string;
  database: string;
  /** SQL Server TLS 加密开关；缺省按 false 处理。 */
  encrypt?: boolean;
  savePassword?: boolean;
  groupName?: string;
  color?: string;
  /** 是否允许内置 MCP 服务访问该连接；缺省视为允许（历史行为）。 */
  mcpAllowed?: boolean;
}

export interface ColumnMetadata {
  name: string;
  dataType: string;
  unifiedType: string;
  nullable: boolean;
  length?: number;
  precision?: number;
  scale?: number;
}

export interface RowData {
  values: (string | number | boolean | null)[];
}

export interface QueryResult {
  columns: ColumnMetadata[];
  rows: RowData[];
  totalCount?: number;
  executionTimeMs: number;
  rowsAffected?: number;
  truncated?: boolean;
  error?: string;
}

/** 执行开始时固定的连接上下文，不随连接树的选择变化。 */
export interface QueryTarget {
  connectionId: string;
  database: string;
  driverType: string;
}

export interface QueryExecution extends QueryTarget {
  taskId: number;
  /** 批量脚本暂不支持后端取消，因此没有 queryId。 */
  queryId?: number;
  sql: string;
  /** 单表结果刷新时使用已解析的完整表名。 */
  refreshSql?: string;
}

/** 数据库解析的真实对象身份；database 可以与查询的默认库不同。 */
export interface ResolvedTable {
  database: string;
  schema?: string | null;
  table: string;
  columns: ColumnInfo[];
}

/** MCP 设置 + 运行状态快照（mcp_settings_get / mcp_settings_save 返回） */
export interface McpSettingsSnapshot {
  enabled: boolean;
  port: number;
  disabledTools: string[];
  running: boolean;
  url?: string | null;
  /** 后端支持的全部工具名 */
  tools: string[];
}

/** MCP 设置的可编辑部分（mcp_settings_save 入参） */
export interface McpSettingsInput {
  enabled: boolean;
  port: number;
  disabledTools: string[];
}

export interface TableInfo {
  name: string;
  schema?: string;
  rowCount?: number;
  engine?: string;
  comment?: string;
}

export interface DatabaseInfo {
  name: string;
  accessible: boolean;
}

/** SQL 转储导入结果汇总 */
export interface ImportSummary {
  statements: number;
  executed: number;
  skipped: number;
}

/** SQL 转储导出结果汇总 */
export interface DumpSummary {
  tables: number;
  rows: number;
}

/** 查询历史记录条目 */
export interface HistoryEntry {
  id: string;
  connectionId: string;
  database?: string;
  sql: string;
  executedAt: number;
  durationMs: number;
  rowCount?: number;
  truncated: boolean;
}

/** 数据库账号信息 */
export interface DbUserInfo {
  name: string;
  host?: string;
  note?: string;
}

/** 当前会话用户及其授权语句 */
export interface CurrentUserInfo {
  user: string;
  grants: string[];
}

export interface ColumnInfo {
  name: string;
  dataType: string;
  nullable: boolean;
  isPrimaryKey: boolean;
  defaultValue?: string;
  extra?: string;
  comment?: string;
}

export interface IndexInfo {
  name: string;
  columns: string[];
  isUnique: boolean;
  isPrimary: boolean;
}

export interface ForeignKeyInfo {
  name: string;
  columns: string[];
  refTable: string;
  refSchema?: string;
  refColumns: string[];
  onUpdate?: string;
  onDelete?: string;
}

export interface TreeNode {
  id: string;
  type: 'connection' | 'database' | 'table' | 'view' | 'column' | 'index' | 'folder' | 'trigger' | 'procedure' | 'function';
  name: string;
  children?: TreeNode[];
  isExpanded?: boolean;
  isLoading?: boolean;
  metadata?: Record<string, unknown>;
}

/** 数据网格编辑上下文：结果集来自哪张表、主键是什么 */
export interface EditContext {
  connectionId: string;
  database: string;
  schema?: string | null;
  table: string;
  primaryKeys: string[];
  driverType: string;
}
