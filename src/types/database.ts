import type { ResultContext } from "./resultContext";
import type { TypedValue } from "./typedValue";

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
  /** 超出 JavaScript 安全范围的标量整数以十进制字符串传输，禁止转回 Number。 */
  values: (string | number | boolean | null)[];
}

export interface QueryResult {
  task?: import("./task").TaskSnapshot;
  columns: ColumnMetadata[];
  rows: RowData[];
  totalCount?: number;
  executionTimeMs: number;
  rowsAffected?: number;
  truncated?: boolean;
  error?: string;
  /** 仅受控后端读取返回；不能从显示列或手工 SQL 自造。 */
  resultContext?: ResultContext;
  rowHandles?: (string | null)[];
  /** 外层 null 表示无法无损读取；SQL NULL 使用 { kind: "null" }。 */
  typedRows?: (TypedValue | null)[][];
}

/** 执行开始时固定的连接上下文，不随连接树的选择变化。 */
export interface QueryTarget {
  connectionId: string;
  database: string;
  driverType: string;
}

export interface QueryExecution extends QueryTarget {
  taskId: number;
  /** 数字 ID 保留兼容，取消同时固定独立 UUID，不能重绑定任务。 */
  queryId?: number;
  backendTaskId?: string;
  canCancel?: boolean;
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
  objectKind?: string | null;
}

/** 分段对象身份；名称里的点号、空格和分隔符都属于名称本身。 */
export interface ObjectIdentity {
  database: string;
  schema?: string | null;
  table: string;
}

export interface MetadataSnapshot {
  connectionId: string;
  connectionGeneration: string;
  revision: string;
  target: ResolvedTable;
  indexes: IndexInfo[];
  foreignKeys: ForeignKeyInfo[];
}

export interface ExecuteDdlRequest {
  connectionId: string;
  connectionGeneration: string;
  database: string;
  tableParts: string[];
  revision: string;
  statements: string[];
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
  cleanupNotice?: string;
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
  keyParts?: { column?: string | null; expression?: string | null; descending: boolean }[];
  includedColumns?: string[];
  definition?: string | null;
  predicate?: string | null;
}

export interface ForeignKeyInfo {
  name: string;
  columns: string[];
  refTable: string;
  refSchema?: string;
  refDatabase?: string;
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
  /** 操作使用结构化来源，id 仅作 UI 键，不再从拼接字符串反解名称。 */
  source?: { connectionId: string; database: string; schema?: string; table?: string };
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
