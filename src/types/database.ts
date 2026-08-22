export interface ConnectionConfig {
  id?: string;
  name: string;
  driverType: 'mysql' | 'mariadb' | 'postgres' | 'postgresql' | 'sqlite' | 'sqlserver' | 'mssql' | (string & {});
  host: string;
  port: number;
  user: string;
  password: string;
  database: string;
  savePassword?: boolean;
  groupName?: string;
  color?: string;
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

export interface TableInfo {
  name: string;
  schema?: string;
  rowCount?: number;
  engine?: string;
  comment?: string;
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
  type: 'connection' | 'database' | 'table' | 'column' | 'index' | 'folder';
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
  table: string;
  primaryKeys: string[];
  driverType: string;
}
