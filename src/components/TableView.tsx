import { useCallback, useEffect, useState } from "react";
import { EditContext, QueryResult } from "@/types/database";
import { tauriService } from "@/services/tauriService";
import { quoteIdent, sqlLiteral, commonDataTypes } from "@/lib/utils";
import { buildAlterStatements, ColumnDraft, DefaultKind, FkDraft, IndexDraft } from "@/lib/ddl";
import { useConnectionStore } from "@/stores/connectionStore";
import { DataGrid } from "@/components/result/DataGrid";
import { Button } from "@/components/ui";
import { cn } from "@/lib/utils";
import { Check, Filter, Loader2, Plus, RefreshCw, RotateCcw, Key, Undo2, X } from "lucide-react";

interface TableViewProps {
  connectionId: string;
  database: string;
  table: string;
}

type SubTab = "data" | "structure";

/** 表视图：数据（可编辑/删除/新增） + 结构（可编辑的表结构设计器） */
export function TableView({ connectionId, database, table }: TableViewProps) {
  const [subTab, setSubTab] = useState<SubTab>("data");

  return (
    <div className="flex flex-col h-full">
      <div className="flex items-center border-b bg-muted/20 px-2">
        {(["data", "structure"] as SubTab[]).map((st) => (
          <button
            key={st}
            className={cn(
              "px-3 py-1.5 text-sm border-b-2 -mb-px",
              subTab === st
                ? "border-blue-500 text-blue-600 font-medium"
                : "border-transparent text-muted-foreground hover:text-foreground"
            )}
            onClick={() => setSubTab(st)}
          >
            {st === "data" ? "数据" : "结构"}
          </button>
        ))}
        <span className="ml-auto text-xs text-muted-foreground pr-2">
          {database} / {table}
        </span>
      </div>

      {subTab === "data" ? (
        <TableData connectionId={connectionId} database={database} table={table} />
      ) : (
        <TableStructure connectionId={connectionId} database={database} table={table} />
      )}
    </div>
  );
}

const PAGE_SIZE_OPTIONS = [100, 500, 1000, 2000];

// ---------- 行筛选 ----------

interface FilterRow {
  column: string;
  op: string;
  value: string;
}

/** 操作符定义：needsValue = false 时不显示值输入框 */
const FILTER_OPS: { value: string; label: string; needsValue: boolean }[] = [
  { value: "=", label: "=", needsValue: true },
  { value: "!=", label: "≠", needsValue: true },
  { value: ">", label: ">", needsValue: true },
  { value: ">=", label: "≥", needsValue: true },
  { value: "<", label: "<", needsValue: true },
  { value: "<=", label: "≤", needsValue: true },
  { value: "contains", label: "包含", needsValue: true },
  { value: "not_contains", label: "不包含", needsValue: true },
  { value: "is_null", label: "IS NULL", needsValue: false },
  { value: "not_null", label: "IS NOT NULL", needsValue: false },
];

/** 草稿筛选 → WHERE 子句（空筛选返回空串）。值经 sqlLiteral 转义，列名经 quoteIdent 引用 */
function buildWhere(filters: FilterRow[], driverType: string): string {
  const clauses: string[] = [];
  for (const f of filters) {
    if (!f.column) continue;
    const col = quoteIdent(f.column, driverType);
    switch (f.op) {
      case "is_null":
        clauses.push(`${col} IS NULL`);
        break;
      case "not_null":
        clauses.push(`${col} IS NOT NULL`);
        break;
      case "contains":
        if (f.value === "") continue;
        clauses.push(`${col} LIKE '%${escapeLikeValue(f.value)}%'`);
        break;
      case "not_contains":
        if (f.value === "") continue;
        clauses.push(`${col} NOT LIKE '%${escapeLikeValue(f.value)}%'`);
        break;
      default:
        if (f.value === "") continue;
        clauses.push(`${col} ${f.op} ${sqlLiteral(f.value)}`);
    }
  }
  return clauses.length > 0 ? ` WHERE ${clauses.join(" AND ")}` : "";
}

/** LIKE 模式串的转义（先转 LIKE 通配符，再走字符串转义由 sqlLiteral 处理引号） */
function escapeLikeValue(v: string): string {
  return v.replace(/'/g, "''");
}

function TableData({ connectionId, database, table }: TableViewProps) {
  const [result, setResult] = useState<QueryResult | undefined>();
  const [editContext, setEditContext] = useState<EditContext | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(100);
  const [totalRows, setTotalRows] = useState<number | null>(null);
  // 筛选：filters 是草稿，appliedFilters 才进入查询（点"应用"或回车生效）
  const [filters, setFilters] = useState<FilterRow[]>([]);
  const [appliedFilters, setAppliedFilters] = useState<FilterRow[]>([]);
  const [colNames, setColNames] = useState<string[]>([]);

  const driverType =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.driverType ??
    "mysql";

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    const qt = quoteIdent(table, driverType);
    const where = buildWhere(appliedFilters, driverType);
    const countSql = `SELECT COUNT(*) AS cnt FROM ${qt}${where}`;
    // SQL Server 用 OFFSET/FETCH（需要 ORDER BY）；其余用 LIMIT/OFFSET
    const dataSql =
      driverType === "sqlserver" || driverType === "mssql"
        ? `SELECT * FROM ${qt}${where} ORDER BY (SELECT 1) OFFSET ${(page - 1) * pageSize} ROWS FETCH NEXT ${pageSize} ROWS ONLY`
        : `SELECT * FROM ${qt}${where} LIMIT ${pageSize} OFFSET ${(page - 1) * pageSize}`;
    try {
      const [countResult, queryResult, cols] = await Promise.all([
        tauriService.executeQuery(connectionId, countSql, database),
        tauriService.executeQuery(connectionId, dataSql, database),
        tauriService.getColumns(connectionId, database, table),
      ]);
      const total = countResult.rows[0]?.values[0];
      setTotalRows(typeof total === "number" ? total : null);
      setResult(queryResult);
      setColNames(cols.map((c) => c.name));
      if (!queryResult.error && queryResult.columns.length > 0) {
        setEditContext({
          connectionId,
          database,
          table,
          primaryKeys: cols.filter((c) => c.isPrimaryKey).map((c) => c.name),
          driverType,
        });
      } else {
        setEditContext(null);
      }
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [connectionId, database, table, page, pageSize, appliedFilters, driverType]);

  useEffect(() => {
    load();
  }, [load]);

  /** 应用草稿筛选：回到第 1 页重新查询 */
  const applyFilters = (next: FilterRow[]) => {
    setFilters(next);
    setAppliedFilters(next.filter((f) => f.column));
    setPage(1);
  };

  const updateFilter = (i: number, patch: Partial<FilterRow>) =>
    setFilters((prev) => prev.map((f, idx) => (idx === i ? { ...f, ...patch } : f)));

  const handleApplyEdits = async (statements: string[]) => {
    await tauriService.executeBatch(connectionId, database, statements);
    await load();
  };

  const totalPages =
    totalRows !== null ? Math.max(1, Math.ceil(totalRows / pageSize)) : 1;

  return (
    <div className="flex flex-col h-full min-h-0">
      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">
          加载失败: {error}
        </div>
      )}

      {/* 筛选栏：列 + 操作符 + 值，多条 AND 组合 */}
      <div className="flex flex-wrap items-center gap-1.5 px-3 py-1.5 border-b text-xs bg-muted/10">
        <Filter className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
        {filters.map((f, i) => {
          const op = FILTER_OPS.find((o) => o.value === f.op) ?? FILTER_OPS[0];
          return (
            <span key={i} className="flex items-center gap-1 rounded border bg-background px-1.5 py-0.5">
              <select
                className="h-5 text-xs bg-transparent focus:outline-none"
                value={f.column}
                onChange={(e) => updateFilter(i, { column: e.target.value })}
              >
                {colNames.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
              <select
                className="h-5 text-xs bg-transparent focus:outline-none text-muted-foreground"
                value={f.op}
                onChange={(e) => updateFilter(i, { op: e.target.value })}
              >
                {FILTER_OPS.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
              </select>
              {op.needsValue && (
                <input
                  className="h-5 w-24 px-1 text-xs bg-transparent focus:outline-none border-l"
                  value={f.value}
                  placeholder="值"
                  onChange={(e) => updateFilter(i, { value: e.target.value })}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") applyFilters(filters);
                  }}
                />
              )}
              <button
                className="text-gray-400 hover:text-red-500"
                title="移除筛选"
                onClick={() => applyFilters(filters.filter((_, idx) => idx !== i))}
              >
                <X className="h-3 w-3" />
              </button>
            </span>
          );
        })}
        <button
          className="flex items-center gap-0.5 text-blue-600 hover:underline"
          disabled={colNames.length === 0}
          onClick={() =>
            setFilters((prev) => [...prev, { column: colNames[0] ?? "", op: "=", value: "" }])
          }
        >
          <Plus className="h-3 w-3" />
          筛选
        </button>
        {filters.length > 0 && (
          <Button size="sm" className="h-5 px-2 text-xs" onClick={() => applyFilters(filters)}>
            应用
          </Button>
        )}
        {appliedFilters.length > 0 && (
          <span className="text-amber-600">已筛选 {appliedFilters.length} 个条件</span>
        )}
      </div>

      <div className="flex-1 min-h-0">
        <DataGrid result={result} editContext={editContext} onApplyEdits={handleApplyEdits} />
      </div>
      {/* 底部分页栏：刷新固定左下，分页控件右对齐 */}
      <div className="flex items-center gap-2 px-3 py-1 border-t text-xs text-muted-foreground">
        <Button variant="ghost" size="sm" className="h-6" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1">刷新</span>
        </Button>
        <span>{totalRows !== null ? `共 ${totalRows.toLocaleString()} 行` : "…"}</span>
        <div className="ml-auto flex items-center gap-2">
          <span>
            第 {page} / {totalPages} 页
          </span>
          <button
            className="px-1.5 py-0.5 rounded border hover:bg-gray-100 disabled:opacity-40"
            disabled={page <= 1 || loading}
            onClick={() => setPage((p) => Math.max(1, p - 1))}
          >
            ‹ 上一页
          </button>
          <button
            className="px-1.5 py-0.5 rounded border hover:bg-gray-100 disabled:opacity-40"
            disabled={page >= totalPages || loading}
            onClick={() => setPage((p) => Math.min(totalPages, p + 1))}
          >
            下一页 ›
          </button>
          <select
            className="h-6 rounded border bg-background px-1 text-xs"
            value={pageSize}
            onChange={(e) => {
              setPageSize(Number(e.target.value));
              setPage(1);
            }}
          >
            {PAGE_SIZE_OPTIONS.map((s) => (
              <option key={s} value={s}>
                每页 {s}
              </option>
            ))}
          </select>
        </div>
      </div>
    </div>
  );
}

type StructTab = "columns" | "indexes" | "fks";

function TableStructure({ connectionId, database, table }: TableViewProps) {
  const [origCols, setOrigCols] = useState<ColumnDraft[]>([]);
  const [cols, setCols] = useState<ColumnDraft[]>([]);
  const [origIdxs, setOrigIdxs] = useState<IndexDraft[]>([]);
  const [idxs, setIdxs] = useState<IndexDraft[]>([]);
  const [origFks, setOrigFks] = useState<FkDraft[]>([]);
  const [fks, setFks] = useState<FkDraft[]>([]);
  const [structTab, setStructTab] = useState<StructTab>("columns");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [showSql, setShowSql] = useState(false);

  const driverType =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.driverType ??
    "mysql";
  const isMysql = driverType === "mysql";
  const dataTypes = commonDataTypes(driverType);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    setNotice(null);
    try {
      const [colInfos, idxInfos, fkInfos] = await Promise.all([
        tauriService.getColumns(connectionId, database, table),
        tauriService.getIndexes(connectionId, database, table),
        tauriService.getForeignKeys(connectionId, database, table),
      ]);
      const colDrafts: ColumnDraft[] = colInfos.map((c) => ({
        name: c.name,
        dataType: c.dataType,
        nullable: c.nullable,
        defaultKind: c.defaultValue == null ? "null" : c.defaultValue === "" ? "empty" : "value",
        defaultValue: c.defaultValue ?? "",
        isPrimary: c.isPrimaryKey,
        autoIncrement: (c.extra ?? "").toLowerCase().includes("auto_increment"),
        comment: c.comment ?? "",
      }));
      const idxDrafts: IndexDraft[] = idxInfos.map((i) => ({
        name: i.name,
        columns: i.columns.join(", "),
        isUnique: i.isUnique,
        isPrimary: i.isPrimary,
      }));
      const fkDrafts: FkDraft[] = fkInfos.map((f) => ({
        name: f.name,
        columns: f.columns.join(", "),
        refTable: f.refTable,
        refColumns: f.refColumns.join(", "),
        onDelete: f.onDelete ?? "",
        onUpdate: f.onUpdate ?? "",
      }));
      setOrigCols(colDrafts);
      setCols(colDrafts.map((c) => ({ ...c })));
      setOrigIdxs(idxDrafts);
      setIdxs(idxDrafts.map((i) => ({ ...i })));
      setOrigFks(fkDrafts);
      setFks(fkDrafts.map((f) => ({ ...f })));
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [connectionId, database, table]);

  useEffect(() => {
    load();
  }, [load]);

  // 实时生成待执行的 DDL 语句（供预览与提交）
  const statements = buildAlterStatements(driverType, table, origCols, cols, origIdxs, idxs, origFks, fks);

  const updateCol = (i: number, patch: Partial<ColumnDraft>) => {
    setCols((prev) => prev.map((c, idx) => (idx === i ? { ...c, ...patch } : c)));
  };

  const handleSubmit = async () => {
    if (statements.length === 0) return;
    setSubmitting(true);
    setError(null);
    try {
      await tauriService.executeBatch(connectionId, database, statements);
      setNotice(`已执行 ${statements.length} 条 DDL`);
      await load();
    } catch (err) {
      setError(`提交失败: ${err}`);
    } finally {
      setSubmitting(false);
    }
  };

  const inputCls =
    "w-full h-7 px-1.5 text-xs rounded border bg-background focus:outline-none focus:ring-1 focus:ring-blue-400";
  const selectCls =
    "h-7 px-1 text-xs rounded border bg-background focus:outline-none focus:ring-1 focus:ring-blue-400";
  const thCls = "px-2 py-1.5 text-left font-semibold text-xs border-b bg-gray-100 dark:bg-gray-800";
  const tdCls = "px-2 py-1 border-b";

  return (
    <div className="flex flex-col h-full min-h-0">
      {/* 操作栏 */}
      <div className="flex items-center gap-2 px-3 py-1.5 border-b text-xs">
        {(["columns", "indexes", "fks"] as StructTab[]).map((st) => (
          <button
            key={st}
            className={cn(
              "px-2.5 py-1 rounded text-xs",
              structTab === st
                ? "subtab-active"
                : "text-muted-foreground hover:text-foreground"
            )}
            onClick={() => setStructTab(st)}
          >
            {st === "columns" ? "列" : st === "indexes" ? "索引" : "外键"}
          </button>
        ))}
        {statements.length > 0 && (
          <span className="text-amber-600 font-medium ml-2">{statements.length} 处待提交</span>
        )}
        <div className="ml-auto flex items-center gap-1.5">
          {statements.length > 0 && (
            <>
              <Button variant="ghost" size="sm" className="h-6 text-xs" onClick={() => setShowSql(!showSql)}>
                {showSql ? "隐藏 SQL" : "预览 SQL"}
              </Button>
              <Button size="sm" className="h-6 text-xs" onClick={handleSubmit} disabled={submitting}>
                {submitting ? <Loader2 className="h-3 w-3 animate-spin" /> : <Check className="h-3 w-3 mr-1" />}
                提交
              </Button>
              <Button variant="outline" size="sm" className="h-6 text-xs" onClick={load} disabled={submitting}>
                <RotateCcw className="h-3 w-3 mr-1" />
                放弃
              </Button>
            </>
          )}
          <Button variant="ghost" size="sm" className="h-6 text-xs" onClick={load} disabled={loading}>
            {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          </Button>
        </div>
      </div>

      {/* 常用类型下拉建议（仍可自由输入） */}
      <datalist id="datatype-list-structure">
        {dataTypes.map((t) => (
          <option key={t} value={t} />
        ))}
      </datalist>

      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">{error}</div>
      )}
      {notice && (
        <div className="px-3 py-1.5 bg-green-50 text-green-700 text-xs border-b border-green-200">{notice}</div>
      )}
      {showSql && statements.length > 0 && (
        <pre className="px-3 py-2 text-xs bg-gray-900 text-green-300 border-b overflow-auto max-h-32">
          {statements.join("\n")}
        </pre>
      )}

      {structTab === "columns" ? (
        <div className="flex-1 overflow-auto custom-scrollbar">
          {/* 列 */}
          <div className="flex items-center justify-between px-3 py-2">
            <span className="text-xs font-semibold text-muted-foreground">列</span>
            <Button
              variant="ghost"
              size="sm"
              className="h-6 text-xs"
              onClick={() =>
                setCols((prev) => [
                  ...prev,
                  {
                    name: "",
                    dataType: isMysql ? "VARCHAR(255)" : "text",
                    nullable: true,
                    defaultKind: "null",
                    defaultValue: "",
                    isPrimary: false,
                    autoIncrement: false,
                    comment: "",
                    isNew: true,
                  },
                ])
              }
            >
              <Plus className="h-3 w-3 mr-1" />
              新增列
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>列名</th>
                <th className={thCls}>类型</th>
                <th className={cn(thCls, "text-center w-14")}>可空</th>
                <th className={thCls}>默认值</th>
                {isMysql && <th className={cn(thCls, "text-center w-20")}>自增</th>}
                <th className={cn(thCls, "text-center w-14")}>主键</th>
                <th className={thCls}>注释</th>
                <th className={cn(thCls, "text-center w-12")}></th>
              </tr>
            </thead>
            <tbody>
              {cols.map((c, i) => {
                const orig = origCols[i];
                const changed =
                  c.isNew ||
                  !orig ||
                  c.name !== orig.name ||
                  c.dataType !== orig.dataType ||
                  c.nullable !== orig.nullable ||
                  c.defaultKind !== orig.defaultKind ||
                  c.defaultValue !== orig.defaultValue ||
                  Boolean(c.isPrimary) !== Boolean(orig.isPrimary) ||
                  Boolean(c.autoIncrement) !== Boolean(orig.autoIncrement) ||
                  c.comment !== orig.comment;
                return (
                  <tr
                    key={i}
                    className={cn(
                      c.deleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400",
                      !c.deleted && c.isNew && "bg-green-50/60 dark:bg-green-900/10",
                      !c.deleted && !c.isNew && changed && "bg-amber-50/60 dark:bg-amber-900/10"
                    )}
                  >
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={c.name}
                        placeholder="列名"
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { name: e.target.value })}
                      />
                    </td>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={c.dataType}
                        placeholder="类型"
                        list="datatype-list-structure"
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { dataType: e.target.value })}
                      />
                    </td>
                    <td className={cn(tdCls, "text-center")}>
                      <input
                        type="checkbox"
                        checked={c.nullable}
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { nullable: e.target.checked })}
                      />
                    </td>
                    <td className={tdCls}>
                      <div className="flex items-center gap-1">
                        <select
                          className={selectCls}
                          value={c.defaultKind}
                          disabled={c.deleted}
                          onChange={(e) => updateCol(i, { defaultKind: e.target.value as DefaultKind })}
                        >
                          <option value="null">(NULL)</option>
                          <option value="empty">(EMPTY)</option>
                          <option value="value">自定义</option>
                        </select>
                        {c.defaultKind === "value" && (
                          <input
                            className={inputCls}
                            value={c.defaultValue}
                            placeholder="如 0、now()、'abc'"
                            disabled={c.deleted}
                            onChange={(e) => updateCol(i, { defaultValue: e.target.value })}
                          />
                        )}
                      </div>
                    </td>
                    {isMysql && (
                      <td className={cn(tdCls, "text-center")}>
                        <input
                          type="checkbox"
                          checked={Boolean(c.autoIncrement)}
                          disabled={c.deleted}
                          onChange={(e) => updateCol(i, { autoIncrement: e.target.checked })}
                        />
                      </td>
                    )}
                    <td className={cn(tdCls, "text-center")}>
                      <input
                        type="checkbox"
                        checked={Boolean(c.isPrimary)}
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { isPrimary: e.target.checked })}
                      />
                    </td>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={c.comment}
                        placeholder="注释"
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { comment: e.target.value })}
                      />
                    </td>
                    <td className={cn(tdCls, "text-center")}>
                      <button
                        title={c.deleted ? "恢复" : "删除列"}
                        onClick={() => updateCol(i, { deleted: !c.deleted })}
                      >
                        {c.deleted ? (
                          <Undo2 className="h-3.5 w-3.5 text-green-600 inline" />
                        ) : (
                          <X className="h-3.5 w-3.5 text-gray-400 hover:text-red-600 inline" />
                        )}
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : structTab === "indexes" ? (
        <div className="flex-1 overflow-auto custom-scrollbar">
          {/* 索引 */}
          <div className="flex items-center justify-between px-3 py-2">
            <span className="text-xs font-semibold text-muted-foreground">索引</span>
            <Button
              variant="ghost"
              size="sm"
              className="h-6 text-xs"
              onClick={() =>
                setIdxs((prev) => [
                  ...prev,
                  { name: "", columns: "", isUnique: false, isNew: true },
                ])
              }
            >
              <Plus className="h-3 w-3 mr-1" />
              新增索引
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>索引名</th>
                <th className={thCls}>列（逗号分隔）</th>
                <th className={cn(thCls, "text-center w-14")}>唯一</th>
                <th className={cn(thCls, "text-center w-12")}></th>
              </tr>
            </thead>
            <tbody>
              {idxs.map((idx, i) => (
                <tr
                  key={i}
                  className={cn(
                    idx.deleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400",
                    !idx.deleted && idx.isNew && "bg-green-50/60 dark:bg-green-900/10"
                  )}
                >
                  <td className={tdCls}>
                    {idx.isNew ? (
                      <input
                        className={inputCls}
                        value={idx.name}
                        placeholder="索引名"
                        disabled={idx.deleted}
                        onChange={(e) =>
                          setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))
                        }
                      />
                    ) : (
                      <span className="flex items-center gap-1.5">
                        {idx.isPrimary && <Key className="h-3 w-3 text-yellow-500" />}
                        {idx.name}
                      </span>
                    )}
                  </td>
                  <td className={tdCls}>
                    {idx.isNew ? (
                      <input
                        className={inputCls}
                        value={idx.columns}
                        placeholder="col1, col2"
                        disabled={idx.deleted}
                        onChange={(e) =>
                          setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, columns: e.target.value } : x)))
                        }
                      />
                    ) : (
                      <span className="text-muted-foreground">{idx.columns}</span>
                    )}
                  </td>
                  <td className={cn(tdCls, "text-center")}>
                    <input
                      type="checkbox"
                      checked={idx.isUnique}
                      disabled={!idx.isNew || idx.deleted}
                      onChange={(e) =>
                        setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, isUnique: e.target.checked } : x)))
                      }
                    />
                  </td>
                  <td className={cn(tdCls, "text-center")}>
                    {idx.isPrimary ? (
                      <span className="text-xs text-gray-400">主键</span>
                    ) : (
                      <button
                        title={idx.deleted ? "恢复" : "删除索引"}
                        onClick={() =>
                          setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, deleted: !x.deleted } : x)))
                        }
                      >
                        {idx.deleted ? (
                          <Undo2 className="h-3.5 w-3.5 text-green-600 inline" />
                        ) : (
                          <X className="h-3.5 w-3.5 text-gray-400 hover:text-red-600 inline" />
                        )}
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="flex-1 overflow-auto custom-scrollbar">
          {/* 外键 */}
          <div className="flex items-center justify-between px-3 py-2">
            <span className="text-xs font-semibold text-muted-foreground">
              外键（已有外键仅支持删除；修改请删旧建新）
            </span>
            <Button
              variant="ghost"
              size="sm"
              className="h-6 text-xs"
              onClick={() =>
                setFks((prev) => [
                  ...prev,
                  { name: "", columns: "", refTable: "", refColumns: "", onDelete: "", onUpdate: "", isNew: true },
                ])
              }
            >
              <Plus className="h-3 w-3 mr-1" />
              新增外键
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>约束名</th>
                <th className={thCls}>本表列</th>
                <th className={thCls}>引用表</th>
                <th className={thCls}>引用列</th>
                <th className={cn(thCls, "w-28")}>ON DELETE</th>
                <th className={cn(thCls, "w-28")}>ON UPDATE</th>
                <th className={cn(thCls, "text-center w-12")}></th>
              </tr>
            </thead>
            <tbody>
              {fks.length === 0 && (
                <tr>
                  <td colSpan={7} className="px-3 py-4 text-center text-xs text-muted-foreground">
                    暂无外键
                  </td>
                </tr>
              )}
              {fks.map((fk, i) => {
                const updateFk = (patch: Partial<FkDraft>) =>
                  setFks((prev) => prev.map((x, j) => (j === i ? { ...x, ...patch } : x)));
                const ruleSelect = (value: string, onChange: (v: string) => void) => (
                  <select
                    className={selectCls + " w-full"}
                    value={value}
                    disabled={!fk.isNew || fk.deleted}
                    onChange={(e) => onChange(e.target.value)}
                  >
                    <option value="">（默认）</option>
                    <option value="CASCADE">CASCADE</option>
                    <option value="SET NULL">SET NULL</option>
                    <option value="RESTRICT">RESTRICT</option>
                    <option value="NO ACTION">NO ACTION</option>
                  </select>
                );
                return (
                  <tr
                    key={i}
                    className={cn(
                      fk.deleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400",
                      !fk.deleted && fk.isNew && "bg-green-50/60 dark:bg-green-900/10"
                    )}
                  >
                    <td className={tdCls}>
                      {fk.isNew ? (
                        <input
                          className={inputCls}
                          value={fk.name}
                          placeholder="fk_xxx"
                          disabled={fk.deleted}
                          onChange={(e) => updateFk({ name: e.target.value })}
                        />
                      ) : (
                        fk.name
                      )}
                    </td>
                    <td className={tdCls}>
                      {fk.isNew ? (
                        <input
                          className={inputCls}
                          value={fk.columns}
                          placeholder="col1, col2"
                          disabled={fk.deleted}
                          onChange={(e) => updateFk({ columns: e.target.value })}
                        />
                      ) : (
                        <span className="text-muted-foreground">{fk.columns}</span>
                      )}
                    </td>
                    <td className={tdCls}>
                      {fk.isNew ? (
                        <input
                          className={inputCls}
                          value={fk.refTable}
                          placeholder="引用表"
                          disabled={fk.deleted}
                          onChange={(e) => updateFk({ refTable: e.target.value })}
                        />
                      ) : (
                        fk.refTable
                      )}
                    </td>
                    <td className={tdCls}>
                      {fk.isNew ? (
                        <input
                          className={inputCls}
                          value={fk.refColumns}
                          placeholder="id"
                          disabled={fk.deleted}
                          onChange={(e) => updateFk({ refColumns: e.target.value })}
                        />
                      ) : (
                        <span className="text-muted-foreground">{fk.refColumns}</span>
                      )}
                    </td>
                    <td className={tdCls}>
                      {fk.isNew ? (
                        ruleSelect(fk.onDelete, (v) => updateFk({ onDelete: v }))
                      ) : (
                        <span className="text-muted-foreground">{fk.onDelete || "—"}</span>
                      )}
                    </td>
                    <td className={tdCls}>
                      {fk.isNew ? (
                        ruleSelect(fk.onUpdate, (v) => updateFk({ onUpdate: v }))
                      ) : (
                        <span className="text-muted-foreground">{fk.onUpdate || "—"}</span>
                      )}
                    </td>
                    <td className={cn(tdCls, "text-center")}>
                      <button
                        title={fk.deleted ? "恢复" : "删除外键"}
                        onClick={() => updateFk({ deleted: !fk.deleted })}
                      >
                        {fk.deleted ? (
                          <Undo2 className="h-3.5 w-3.5 text-green-600 inline" />
                        ) : (
                          <X className="h-3.5 w-3.5 text-gray-400 hover:text-red-600 inline" />
                        )}
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {/* 底栏：刷新固定左下 */}
      <div className="flex items-center gap-2 px-3 py-1 border-t text-xs text-muted-foreground">
        <Button variant="ghost" size="sm" className="h-6" onClick={load} disabled={loading}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1">刷新</span>
        </Button>
        <span className="text-muted-foreground/60">
          {database} / {table}
        </span>
      </div>
    </div>
  );
}
