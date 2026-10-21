import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DatasetController } from "@/lib/datasetController";
import { EditContext, MetadataSnapshot, QueryResult } from "@/types/database";
import { metadataCache } from "@/lib/metadataCache";
import { tauriService } from "@/services/tauriService";
import { quoteIdent, sqlLiteral, commonDataTypes } from "@/lib/utils";
import { quoteTable, tableParts } from "@/lib/sqlTable";
import { buildPageSql } from "@/lib/tablePaging";
import { attachResultContext, formatResultContextError, retireResultContext } from "@/lib/resultContext";
import { guardGridDraft, tableDraftKey, useGridDraftStore } from "@/stores/gridDraftStore";
import type { ControlledTableReadRequest, ResultContext } from "@/types/resultContext";
import { buildAlterStatements, ColumnDraft, DefaultKind, FkDraft, IndexDraft } from "@/lib/ddl";
import { useConnectionStore } from "@/stores/connectionStore";
import { DataGrid, GridSort } from "@/components/result/DataGrid";
import { Button } from "@/components/ui";
import { cn } from "@/lib/utils";
import { Check, Filter, Loader2, Plus, RefreshCw, RotateCcw, Key, Undo2, X } from "lucide-react";
import { useT } from "@/i18n";
import type { TableSubTab } from "@/stores/workspaceStore";

interface TableViewProps {
  connectionId: string;
  database: string;
  schema?: string;
  table: string;
  /** 视图等只读对象：禁用网格编辑 */
  readonly?: boolean;
  /** 初始子页（数据/结构），由打开入口（如右键菜单）指定 */
  initialSubTab?: TableSubTab;
  /** 已打开的 tab 被再次指定子页时递增，触发子页切换 */
  subTabNonce?: number;
}

type SubTab = TableSubTab;

/** 表视图：后端授权的受控数据浏览与表结构设计器。 */
export function TableView({ connectionId, database, schema, table, readonly, initialSubTab, subTabNonce }: TableViewProps) {
  const t = useT();
  const [subTab, setSubTab] = useState<SubTab>(initialSubTab ?? "data");
  const draftKey = tableDraftKey(connectionId, database, table, schema);

  // tab 已处于打开状态时，右键「查数据 / 编辑结构」通过 nonce 驱动子页切换
  useEffect(() => {
    if (subTabNonce !== undefined && initialSubTab && initialSubTab !== subTab) {
      void guardGridDraft(draftKey).then((allowed) => { if (allowed) setSubTab(initialSubTab); });
    }
    // 只在 nonce 变化时同步，用户手动切换子页不受影响
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subTabNonce]);

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
            onClick={async () => { if (st !== subTab && await guardGridDraft(draftKey)) setSubTab(st); }}
          >
            {st === "data" ? t("tableView.data") : t("tableView.structure")}
          </button>
        ))}
        <span className="ml-auto text-xs text-muted-foreground pr-2">
          {[database, schema, table].filter(Boolean).join(" / ")}
        </span>
      </div>

      {subTab === "data" ? (
        <TableData connectionId={connectionId} database={database} schema={schema} table={table} readonly={readonly} />
      ) : (
        <TableStructure connectionId={connectionId} database={database} schema={schema} table={table} />
      )}
    </div>
  );
}

const PAGE_SIZE_OPTIONS = [20, 50, 100];
let filteredTaskSequence = Date.now() * 1000;

// ---------- 行筛选 ----------

interface FilterRow {
  column: string;
  op: string;
  value: string;
}

/** 操作符定义：needsValue = false 时不显示值输入框；labelKey 为 None 时直接用 value 作符号 */
const FILTER_OPS = [
  { value: "=", labelKey: null, needsValue: true },
  { value: "!=", label: "≠", labelKey: null, needsValue: true },
  { value: ">", labelKey: null, needsValue: true },
  { value: ">=", label: "≥", labelKey: null, needsValue: true },
  { value: "<", labelKey: null, needsValue: true },
  { value: "<=", label: "≤", labelKey: null, needsValue: true },
  { value: "contains", labelKey: "tableView.opContains", needsValue: true },
  { value: "not_contains", labelKey: "tableView.opNotContains", needsValue: true },
  { value: "is_null", label: "IS NULL", labelKey: null, needsValue: false },
  { value: "not_null", label: "IS NOT NULL", labelKey: null, needsValue: false },
] as const;

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

export function TableData({ connectionId, database, schema, table, readonly }: TableViewProps) {
  const t = useT();
  const draftKey = tableDraftKey(connectionId, database, table, schema);
  const [result, setResult] = useState<QueryResult | undefined>();
  const [editContext, setEditContext] = useState<EditContext | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [page, setPage] = useState(1);
  const [displayPage, setDisplayPage] = useState(1);
  const [jumpPage, setJumpPage] = useState("");
  const [pageSize, setPageSize] = useState(20);
  // 排序下推数据库：只排当前页没有意义，翻页/筛选后仍保持该排序
  const [sort, setSort] = useState<GridSort | null>(null);
  const [totalRows, setTotalRows] = useState<number | null>(null);
  const [totalText, setTotalText] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [paginationMode, setPaginationMode] = useState("offset");
  const [paginationFallback, setPaginationFallback] = useState<string | null>(null);
  const [counting, setCounting] = useState(false);
  const [countError, setCountError] = useState<string | null>(null);
  const [cancelRequested, setCancelRequested] = useState(false);
  const [canCancel, setCanCancel] = useState(false);
  const rawTasks = useRef(new Map<string, number>());
  const filteredCountSql = useRef<string | null>(null);
  // 筛选：filters 是草稿，appliedFilters 才进入查询（点"应用"或回车生效）
  const [filters, setFilters] = useState<FilterRow[]>([]);
  const [appliedFilters, setAppliedFilters] = useState<FilterRow[]>([]);
  const [colNames, setColNames] = useState<string[]>([]);
  const loadSequence = useRef(0);
  const currentResult = useRef<QueryResult>();
  const currentSpec = useRef("");
  const connectionEpoch = useConnectionStore((state) => state.connectionEpochs[connectionId] ?? 0);
  const viewSource = JSON.stringify([connectionId, connectionEpoch, database, schema ?? null, table]);
  const acceptedSource = useRef("");

  const retire = useCallback((previous: QueryResult | undefined) => {
    void retireResultContext(previous?.resultContext, Object.values(useGridDraftStore.getState().drafts),
      (resultId, revision) => tauriService.releaseResultContext(resultId, revision));
  }, []);
  const dataset = useMemo(() => {
    const cancel = async (taskId: string) => {
      const queryId = rawTasks.current.get(taskId);
      if (queryId !== undefined) await tauriService.cancelQuery(connectionId, queryId, taskId);
      else await tauriService.cancelTableTask(connectionId, taskId);
    };
    return new DatasetController({ cancelPage: cancel, cancelCount: cancel, retire });
  }, [connectionId, retire]);
  useEffect(() => { dataset.activate(); return () => dataset.dispose(); }, [dataset]);
  useEffect(() => {
    let current = true;
    setCanCancel(false);
    void tauriService.getSessionCapabilities(connectionId).then((capabilities) => {
      if (current) setCanCancel(capabilities.cancellation);
    }).catch(() => undefined);
    return () => { current = false; };
  }, [connectionId, connectionEpoch]);

  // 请求依赖变化只作废请求；只有真正卸载才释放当前显示的干净结果。
  useEffect(() => () => {
    loadSequence.current++;
    retire(currentResult.current);
    currentResult.current = undefined;
  }, [retire]);

  // 同 ID 的断开与重连也必须使旧结果失效；脏草稿由 store 继续固定保留。
  useEffect(() => {
    loadSequence.current++;
    dataset.reset("");
    retire(currentResult.current);
    currentResult.current = undefined;
    setResult(undefined);
    setEditContext(null);
    setTotalRows(null);
    setTotalText(null); setHasMore(false); setCountError(null); setCounting(false);
    setColNames([]);
    setLoading(false);
    setError(null);
  }, [connectionId, connectionEpoch, retire, dataset]);

  const driverType =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.driverType ??
    "mysql";

  const load = useCallback(async (source?: EditContext, afterCommit = false, force = false) => {
    const connectionState = useConnectionStore.getState();
    const requestEpoch = connectionEpoch;
    // epoch 0 允许首次读取；显式断开以后只能在明确恢复 live 后发起读取。
    if ((connectionState.connectionEpochs[connectionId] ?? 0) !== requestEpoch ||
      (requestEpoch > 0 && !connectionState.liveIds.has(connectionId))) {
      if (afterCommit) throw new Error("连接会话已变化，请恢复连接后重试读取。");
      return;
    }
    const sequence = ++loadSequence.current;
    const isCurrent = () => sequence === loadSequence.current &&
      (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) === requestEpoch;
    setLoading(true);
    setError(null);
    setCounting(false); setCancelRequested(false);
    const sourceKey = JSON.stringify([connectionId, requestEpoch, source?.database ?? database, source?.schema ?? schema, source?.table ?? table, pageSize, sort, appliedFilters]);
    if (currentSpec.current !== sourceKey || afterCommit || force) {
      setTotalRows(null); setTotalText(null); setCountError(null);
      currentSpec.current = sourceKey;
    }
    const ticket = dataset.beginPage(sourceKey, page, afterCommit || force);
    const where = buildWhere(appliedFilters, driverType);
    // SQL Server 的 COUNT(*) 返回 int，表超过 21 亿行会溢出（错误 8115）；用 COUNT_BIG 返回 bigint
    const countExpr =
      driverType === "sqlserver" || driverType === "mssql" ? "COUNT_BIG(*)" : "COUNT(*)";
    try {
      if (appliedFilters.length === 0) {
        const request: ControlledTableReadRequest = {
          protocolVersion: 1, connectionId, database: source?.database ?? database,
          tableParts: source ? tableParts(source) : tableParts({ driverType, database, schema, table }), page, pageSize,
          sort: sort ? { column: sort.key, direction: sort.direction } : null,
        };
        const response = await tauriService.readTablePage(request, { taskId: ticket.taskId, cursor: ticket.cursor });
        if (!isCurrent()) {
          void retireResultContext(response.context, [], (resultId, revision) => tauriService.releaseResultContext(resultId, revision));
          if (afterCommit) throw new Error("刷新请求已失效，请重试读取。");
          return;
        }
        let nextResult: QueryResult;
        try {
          nextResult = attachResultContext(response, request);
        } catch (error) {
          void retireResultContext(response.context, [], (resultId, revision) => tauriService.releaseResultContext(resultId, revision));
          throw error;
        }
        if (!dataset.accept(ticket, nextResult, response.nextCursor)) return;
        currentResult.current = nextResult;
        acceptedSource.current = viewSource;
        setDisplayPage(page);
        setResult(nextResult);
        if (response.totalRows !== null) {
          setTotalRows(response.totalRows); setTotalText(String(response.totalRows));
        }
        setHasMore(response.hasMore ?? (response.totalRows !== null ? page * pageSize < response.totalRows : nextResult.rows.length === pageSize));
        setPaginationMode(response.paginationMode ?? "offset");
        setPaginationFallback(response.paginationFallbackReason ?? null);
        filteredCountSql.current = null;
        setColNames(response.table.columns.map((column) => column.name));
        // 网格权限仅由受控结果的后端策略决定。
        setEditContext(null);
        return nextResult;
      }
      // 筛选沿用只读查询；在 P102 类型化绑定前不登记可信上下文。
      const metadata = await tauriService.resolveTable(
        connectionId, source?.database ?? database, source ? tableParts(source) : tableParts({ driverType, database, schema, table })
      );
      if (!isCurrent()) return;
      const qt = quoteTable({ ...metadata, driverType });
      const cols = metadata.columns;
      // 使用同一份已解析身份构造分页、统计和保存目标。
      const countSql = `SELECT ${countExpr} AS cnt FROM ${qt}${where}`;
      const dataSql = buildPageSql(driverType, qt, where, page, pageSize, cols, sort, 1);
      const queryId = ++filteredTaskSequence;
      rawTasks.current.set(ticket.taskId, queryId);
      const rawResult = await tauriService.executeQuery(connectionId, dataSql, database, queryId, false, ticket.taskId);
      if (!isCurrent()) return;
      const { resultContext: _untrustedContext, rowHandles: _untrustedHandles, typedRows: _untrustedValues, ...rawPage } = rawResult;
      const more = rawPage.rows.length > pageSize;
      const queryResult = { ...rawPage, rows: rawPage.rows.slice(0, pageSize) };
      if (!dataset.accept(ticket, queryResult)) return;
      filteredCountSql.current = countSql;
      setHasMore(more); setPaginationMode("offset"); setPaginationFallback("FILTERED_OFFSET");
      currentResult.current = queryResult;
      acceptedSource.current = viewSource;
      setDisplayPage(page);
      setResult(queryResult);
      setColNames(cols.map((c) => c.name));
      if (!readonly && !queryResult.error && queryResult.columns.length > 0) {
        setEditContext({
          connectionId,
          database: metadata.database,
          schema: metadata.schema,
          table: metadata.table,
          primaryKeys: cols.filter((c) => c.isPrimaryKey).map((c) => c.name),
          driverType,
        });
      } else {
        setEditContext(null);
      }
      return queryResult;
    } catch (err) {
      if (isCurrent()) setError(formatResultContextError(err));
      if (afterCommit) throw err;
    } finally {
      rawTasks.current.delete(ticket.taskId);
      dataset.finishPage(ticket);
      if (isCurrent()) setLoading(false);
    }
  }, [connectionId, connectionEpoch, database, schema, table, page, pageSize, sort, appliedFilters, driverType, readonly, retire, dataset]);

  const countRows = async () => {
    if (!dataset.ownsResult(currentResult.current) || acceptedSource.current !== viewSource ||
        (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) !== connectionEpoch) return;
    const context = currentResult.current?.resultContext;
    const sql = filteredCountSql.current;
    if (!context && !sql) return;
    const ticket = dataset.beginCount(context);
    const epoch = connectionEpoch;
    const current = () => dataset.isCountCurrent(ticket) &&
      (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) === epoch;
    setCounting(true); setCancelRequested(false); setCountError(null);
    try {
      let text: string;
      if (context) {
        const response = await tauriService.countTable({ protocolVersion: 1, resultId: context.resultId, revision: context.revision, taskId: ticket.taskId });
        if (response.resultId !== context.resultId || response.revision !== context.revision ||
          response.connectionId !== context.connectionId || response.connectionGeneration !== context.connectionGeneration) {
          throw new Error("统计来源与当前结果不一致，请重新统计。");
        }
        text = response.totalRows;
      } else {
        const queryId = ++filteredTaskSequence;
        rawTasks.current.set(ticket.taskId, queryId);
        const response = await tauriService.executeQuery(connectionId, sql!, database, queryId, false, ticket.taskId);
        const value = response.rows[0]?.values[0];
        if (response.truncated || (typeof value !== "string" && typeof value !== "number")) throw new Error("统计未完整返回。");
        text = String(value);
      }
      if (!/^(0|[1-9]\d*)$/.test(text)) throw new Error("统计结果不是有效的完整行数。");
      if (!current()) return;
      setTotalText(text);
      const count = Number(text);
      setTotalRows(Number.isSafeInteger(count) ? count : null);
    } catch (error) { if (current()) setCountError(formatResultContextError(error)); }
    finally { rawTasks.current.delete(ticket.taskId); if (current()) setCounting(false); dataset.finishCount(ticket); }
  };

  const cancelDatasetTask = async (kind: "page" | "count") => {
    const sequence = loadSequence.current;
    const epoch = connectionEpoch;
    setCancelRequested(true);
    try { await dataset.requestCancel(kind); }
    catch (error) {
      if (sequence === loadSequence.current && (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) === epoch) setCountError(formatResultContextError(error));
    }
  };

  const goToPage = async (next: number) => {
    if (!Number.isSafeInteger(next) || next < 1 || next > 4_294_967_295) {
      setError("页码须为 1 至 4294967295 的整数。"); return;
    }
    if (!await guardGridDraft(draftKey)) return;
    if (next === page) void load(); else setPage(next);
  };

  /** 表头排序：回到第 1 页看排序后的最前数据 */
  const handleSortChange = useCallback((next: GridSort) => {
    void guardGridDraft(draftKey).then((allowed) => {
      if (!allowed) return;
      setSort(next);
      setPage(1);
    });
  }, [draftKey]);

  useEffect(() => {
    void load();
    return () => {
      loadSequence.current++;
    };
  }, [load]);

  /** 应用草稿筛选：回到第 1 页重新查询 */
  const applyFilters = async (next: FilterRow[]) => {
    if (!await guardGridDraft(draftKey)) return;
    setFilters(next);
    setAppliedFilters(next.filter((f) => f.column));
    setPage(1);
  };

  const updateFilter = (i: number, patch: Partial<FilterRow>) =>
    setFilters((prev) => prev.map((f, idx) => (idx === i ? { ...f, ...patch } : f)));

  const refreshCommitted = useCallback(async (source: ResultContext) => {
    if (source.connectionId !== connectionId) throw new Error("刷新来源与原连接不一致。");
    const refreshed = await load({ connectionId, database: source.source.database, schema: source.source.schema,
      table: source.source.table, primaryKeys: [], driverType }, true);
    if (!refreshed) throw new Error("未获得新的受控读取结果，请重试读取。");
    return refreshed;
  }, [connectionId, driverType, load]);

  const totalPages =
    totalRows !== null ? Math.max(1, Math.ceil(totalRows / pageSize)) : 1;

  const opLabel = (o: (typeof FILTER_OPS)[number]) =>
    o.labelKey ? t(o.labelKey) : "label" in o ? o.label : o.value;

  return (
    <div className="flex flex-col h-full min-h-0">
      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">
          {t("common.loadFailed", { error })}
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
                    {opLabel(o)}
                  </option>
                ))}
              </select>
              {op.needsValue && (
                <input
                  className="h-5 w-24 px-1 text-xs bg-transparent focus:outline-none border-l"
                  value={f.value}
                  placeholder={t("tableView.filterValue")}
                  onChange={(e) => updateFilter(i, { value: e.target.value })}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") applyFilters(filters);
                  }}
                />
              )}
              <button
                className="text-gray-400 hover:text-red-500"
                title={t("tableView.removeFilter")}
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
          {t("tableView.addFilter")}
        </button>
        {filters.length > 0 && (
          <Button size="sm" className="h-5 px-2 text-xs" onClick={() => applyFilters(filters)}>
            {t("tableView.apply")}
          </Button>
        )}
        {appliedFilters.length > 0 && (
          <span className="text-amber-600">{t("tableView.filtered", { count: appliedFilters.length })}</span>
        )}
      </div>

      <div className="flex-1 min-h-0">
        <DataGrid
          draftKey={draftKey}
          readOnlyReason={readonly ? "视图结果为只读。" : loading ? "正在读取页面，请等待读取完成。" : result && !dataset.ownsResult(result) ? "当前显示为旧页面，身份已失效，请刷新成功后再编辑。" : undefined}
          result={result}
          editContext={editContext}
          onRefreshCommitted={refreshCommitted}
          sortState={sort}
          onSortChange={handleSortChange}
        />
      </div>
      {/* 底部分页栏：刷新固定左下，分页控件右对齐 */}
      <div className="flex flex-wrap items-center gap-2 px-3 py-1 border-t text-xs text-muted-foreground">
        <Button variant="ghost" size="sm" className="h-6" onClick={async () => { if (await guardGridDraft(draftKey)) void load(undefined, false, true); }} disabled={loading}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1">{t("common.refresh")}</span>
        </Button>
        <span>{totalText !== null ? t("tableView.totalRows", { count: BigInt(totalText).toLocaleString() }) : "总行数未统计"}</span>
        <Button variant="ghost" size="sm" disabled={loading || counting || !dataset.ownsResult(result)} onClick={() => void countRows()}>{counting ? "统计中…" : "统计总行数"}</Button>
        {counting && <Button variant="outline" size="sm" disabled={!canCancel || cancelRequested} onClick={() => void cancelDatasetTask("count")}>{cancelRequested ? "已请求取消" : "取消统计"}</Button>}
        {loading && <Button variant="outline" size="sm" disabled={!canCancel || cancelRequested} onClick={() => void cancelDatasetTask("page")}>{cancelRequested ? "已请求取消" : "取消读取"}</Button>}
        {countError && <span className="text-red-600" role="status">{countError}</span>}
        <div className="ml-auto flex items-center gap-2">
          <span>{totalRows !== null ? t("tableView.pageInfo", { page: displayPage, total: totalPages }) : `第 ${displayPage} 页 · 总数未知`}</span>
          <span className="text-muted-foreground" title={`${paginationFallback ? ({ KEYSET_SOURCE_NOT_BASE_TABLE: "该对象不是基础表。", KEYSET_NO_UNIQUE_PRIMARY_KEY: "没有可靠的完整主键。", KEYSET_NULLABLE_ORDER: "排序列允许空值。", KEYSET_ORDER_TYPE_UNSUPPORTED: "排序类型暂不支持安全游标。", FILTERED_OFFSET: "筛选结果使用偏移分页。" } as Record<string, string>)[paginationFallback] ?? "当前排序使用偏移分页。" : ""}各页与精确统计独立读取，并发修改可能使跨页重复或遗漏。`}>{paginationMode === "keyset" ? "游标续页" : "偏移分页"}</span>
          <button
            className="px-1.5 py-0.5 rounded border hover:bg-gray-100 disabled:opacity-40"
            disabled={displayPage <= 1 || loading}
            onClick={() => void goToPage(Math.max(1, displayPage - 1))}
          >
            {t("tableView.prevPage")}
          </button>
          <button
            className="px-1.5 py-0.5 rounded border hover:bg-gray-100 disabled:opacity-40"
            disabled={!hasMore || loading}
            onClick={() => void goToPage(displayPage + 1)}
          >
            {t("tableView.nextPage")}
          </button>
          <select
            className="h-6 rounded border bg-background px-1 text-xs"
            value={pageSize}
            onChange={async (e) => {
              const nextSize = Number(e.target.value);
              if (!await guardGridDraft(draftKey)) return;
              setPageSize(nextSize);
              setPage(1);
            }}
          >
            {PAGE_SIZE_OPTIONS.map((s) => (
              <option key={s} value={s}>
                {t("tableView.perPage", { size: s })}
              </option>
            ))}
          </select>
          <input aria-label="跳到页码" type="number" min="1" className="w-16 h-6 rounded border bg-background px-1" value={jumpPage} onChange={(event) => setJumpPage(event.target.value)} disabled={loading} />
          <Button variant="outline" size="sm" disabled={loading || !jumpPage} onClick={() => void goToPage(Number(jumpPage))}>跳转</Button>
        </div>
      </div>
    </div>
  );
}

type StructTab = "columns" | "indexes" | "fks";

export function TableStructure({ connectionId, database, schema, table }: TableViewProps) {
  const t = useT();
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
  const [snapshot, setSnapshot] = useState<MetadataSnapshot | null>(null);
  const loadSequence = useRef(0);
  const connectionEpoch = useConnectionStore((state) => state.connectionEpochs[connectionId] ?? 0);

  const driverType =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.driverType ??
    "mysql";
  const isMysql = driverType === "mysql";
  const dataTypes = commonDataTypes(driverType);
  const sourceKey = JSON.stringify([connectionId, connectionEpoch, database, schema ?? null, table, driverType]);
  const currentSource = useRef(sourceKey);
  const sourceSession = useRef(0);
  if (currentSource.current !== sourceKey) sourceSession.current++;
  currentSource.current = sourceKey;
  const snapshotSource = useRef<string | null>(null);

  const load = useCallback(async (refresh = false) => {
    const state = useConnectionStore.getState();
    if (currentSource.current !== sourceKey || (state.connectionEpochs[connectionId] ?? 0) !== connectionEpoch ||
        (connectionEpoch > 0 && !state.liveIds.has(connectionId))) return;
    const sequence = ++loadSequence.current;
    const isCurrent = () => currentSource.current === sourceKey && sequence === loadSequence.current &&
      (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) === connectionEpoch;
    setLoading(true);
    setError(null);
    setNotice(null);
    try {
      const metadata = await metadataCache.get({ connectionId, connectionEpoch, database, schema, table, category: "table-structure" },
        () => tauriService.getTableMetadata(connectionId, database, tableParts({ driverType, database, schema, table })), refresh);
      if (!isCurrent()) return;
      if (metadata.connectionId !== connectionId) throw new Error("结构元数据的连接来源不一致。");
      const colDrafts: ColumnDraft[] = metadata.target.columns.map((c) => ({
        name: c.name,
        dataType: c.dataType,
        nullable: c.nullable,
        defaultKind: c.defaultValue == null ? "null" : c.defaultValue === "" ? "empty" : "value",
        defaultValue: c.defaultValue ?? "",
        isPrimary: c.isPrimaryKey,
        autoIncrement: (c.extra ?? "").toLowerCase().includes("auto_increment"),
        comment: c.comment ?? "",
      }));
      const formatColumns = (names: string[]) => names.map((name) => quoteIdent(name, driverType)).join(", ");
      const idxDrafts: IndexDraft[] = metadata.indexes.map((i) => ({
        name: i.name,
        columns: i.keyParts?.length ? i.keyParts.map((part) =>
          `${part.column != null ? quoteIdent(part.column, driverType) : part.expression ?? "（表达式，详见定义）"}${part.descending ? " DESC" : ""}`).join(", ") : formatColumns(i.columns),
        isUnique: i.isUnique,
        isPrimary: i.isPrimary,
        details: [i.includedColumns?.length ? `INCLUDE (${formatColumns(i.includedColumns)})` : "",
          i.predicate ? `WHERE ${i.predicate}` : "", i.definition ?? ""].filter(Boolean).join("\n"),
      }));
      const fkDrafts: FkDraft[] = metadata.foreignKeys.map((f) => ({
        name: f.name,
        columns: formatColumns(f.columns),
        refTable: f.refTable,
        refSchema: f.refSchema,
        refDatabase: f.refDatabase,
        refColumns: formatColumns(f.refColumns),
        onDelete: f.onDelete ?? "",
        onUpdate: f.onUpdate ?? "",
      }));
      setOrigCols(colDrafts);
      setCols(colDrafts.map((c) => ({ ...c })));
      setOrigIdxs(idxDrafts);
      setIdxs(idxDrafts.map((i) => ({ ...i })));
      setOrigFks(fkDrafts);
      setFks(fkDrafts.map((f) => ({ ...f })));
      snapshotSource.current = sourceKey;
      setSnapshot(metadata);
    } catch (err) {
      if (isCurrent()) setError(String(err));
    } finally {
      if (isCurrent()) setLoading(false);
    }
  }, [connectionId, connectionEpoch, database, schema, table, driverType, sourceKey]);

  useEffect(() => {
    snapshotSource.current = null;
    setSnapshot(null);
    setOrigCols([]); setCols([]); setOrigIdxs([]); setIdxs([]); setOrigFks([]); setFks([]);
    setLoading(false); setSubmitting(false); setError(null); setNotice(null);
    void load();
    return () => { loadSequence.current++; };
  }, [load]);

  // 实时生成待执行的 DDL 语句（供预览与提交）
  let statements: string[] = [];
  let ddlError: string | null = null;
  try {
    if (snapshot && snapshotSource.current === sourceKey) statements = buildAlterStatements(driverType,
      snapshot.target, origCols, cols, origIdxs, idxs, origFks, fks);
  } catch (err) { ddlError = String(err); }

  const updateCol = (i: number, patch: Partial<ColumnDraft>) => {
    setCols((prev) => prev.map((c, idx) => (idx === i ? { ...c, ...patch } : c)));
  };

  const handleSubmit = async () => {
    if (statements.length === 0 || !snapshot || snapshotSource.current !== sourceKey || submitting) return;
    // 提交固定为生成草稿时的后端身份，不读取连接树的当前选择。
    const frozenSnapshot = snapshot;
    const frozenSource = sourceKey;
    const frozenSession = sourceSession.current;
    const frozenStatements = [...statements];
    const isCurrent = () => currentSource.current === frozenSource && sourceSession.current === frozenSession &&
      (useConnectionStore.getState().connectionEpochs[connectionId] ?? 0) === connectionEpoch;
    setSubmitting(true);
    setError(null);
    try {
      await tauriService.executeDdl({ connectionId: frozenSnapshot.connectionId,
        connectionGeneration: frozenSnapshot.connectionGeneration, database: frozenSnapshot.target.database,
        tableParts: tableParts({ ...frozenSnapshot.target, driverType }), revision: frozenSnapshot.revision,
        statements: frozenStatements });
      if (!isCurrent()) return;
      await load(true);
      if (isCurrent()) setNotice(t("tableView.ddlExecuted", { count: frozenStatements.length }));
    } catch (err) {
      if (isCurrent()) setError(t("tableView.submitFailed", { error: String(err) }));
    } finally {
      if (isCurrent()) setSubmitting(false);
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
            {st === "columns" ? t("tableView.columns") : st === "indexes" ? t("tableView.indexes") : t("tableView.fks")}
          </button>
        ))}
        {statements.length > 0 && (
          <span className="text-amber-600 font-medium ml-2">{t("grid.pending", { count: statements.length })}</span>
        )}
        <div className="ml-auto flex items-center gap-1.5">
          {statements.length > 0 && (
            <>
              <Button variant="ghost" size="sm" className="h-6 text-xs" onClick={() => setShowSql(!showSql)}>
                {showSql ? t("tableView.hideSql") : t("tableView.previewSql")}
              </Button>
              <Button size="sm" className="h-6 text-xs" onClick={handleSubmit} disabled={submitting || loading}>
                {submitting ? <Loader2 className="h-3 w-3 animate-spin" /> : <Check className="h-3 w-3 mr-1" />}
                {t("grid.submit")}
              </Button>
              <Button variant="outline" size="sm" className="h-6 text-xs" onClick={() => void load(true)} disabled={submitting}>
                <RotateCcw className="h-3 w-3 mr-1" />
                {t("grid.discard")}
              </Button>
            </>
          )}
          <Button variant="ghost" size="sm" className="h-6 text-xs" onClick={() => void load(true)} disabled={loading || submitting}>
            {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          </Button>
        </div>
      </div>

      {/* 常用类型下拉建议（仍可自由输入） */}
      <datalist id="datatype-list-structure">
        {dataTypes.map((dt) => (
          <option key={dt} value={dt} />
        ))}
      </datalist>

      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">{error}</div>
      )}
      {ddlError && <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">{ddlError}</div>}
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
            <span className="text-xs font-semibold text-muted-foreground">{t("tableView.columns")}</span>
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
              {t("tableView.addColumn")}
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>{t("tableView.thName")}</th>
                <th className={thCls}>{t("tableView.thType")}</th>
                <th className={cn(thCls, "text-center w-14")}>{t("tableView.thNullable")}</th>
                <th className={thCls}>{t("tableView.thDefault")}</th>
                {isMysql && <th className={cn(thCls, "text-center w-20")}>{t("tableView.thAutoInc")}</th>}
                <th className={cn(thCls, "text-center w-14")}>{t("tableView.thPk")}</th>
                <th className={thCls}>{t("tableView.thComment")}</th>
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
                        placeholder={t("tableView.thName")}
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { name: e.target.value })}
                      />
                    </td>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={c.dataType}
                        placeholder={t("tableView.thType")}
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
                          <option value="value">{t("tableView.customDefault")}</option>
                        </select>
                        {c.defaultKind === "value" && (
                          <input
                            className={inputCls}
                            value={c.defaultValue}
                            placeholder={t("tableView.defaultPlaceholder")}
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
                        placeholder={t("tableView.thComment")}
                        disabled={c.deleted}
                        onChange={(e) => updateCol(i, { comment: e.target.value })}
                      />
                    </td>
                    <td className={cn(tdCls, "text-center")}>
                      <button
                        title={c.deleted ? t("tableView.restore") : t("tableView.deleteColumn")}
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
            <span className="text-xs font-semibold text-muted-foreground">{t("tableView.indexes")}</span>
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
              {t("tableView.addIndex")}
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>{t("tableView.thIndexName")}</th>
                <th className={thCls}>{t("tableView.thIndexCols")}</th>
                <th className={cn(thCls, "text-center w-14")}>{t("tableView.thUnique")}</th>
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
                        placeholder={t("tableView.thIndexName")}
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
                      <span className="text-muted-foreground whitespace-pre-wrap" title={idx.details}>
                        {idx.columns}{idx.details && <small className="block">{idx.details}</small>}
                      </span>
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
                      <span className="text-xs text-gray-400">{t("tableView.thPk")}</span>
                    ) : (
                      <button
                        title={idx.deleted ? t("tableView.restore") : t("tableView.deleteIndex")}
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
              {t("tableView.fkNote")}
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
              {t("tableView.addFk")}
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>{t("tableView.thConstraint")}</th>
                <th className={thCls}>{t("tableView.thColumns")}</th>
                <th className={thCls}>{t("tableView.thRefTable")}</th>
                <th className={thCls}>{t("tableView.thRefColumns")}</th>
                <th className={cn(thCls, "w-28")}>ON DELETE</th>
                <th className={cn(thCls, "w-28")}>ON UPDATE</th>
                <th className={cn(thCls, "text-center w-12")}></th>
              </tr>
            </thead>
            <tbody>
              {fks.length === 0 && (
                <tr>
                  <td colSpan={7} className="px-3 py-4 text-center text-xs text-muted-foreground">
                    {t("tableView.noFks")}
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
                    <option value="">{t("tableView.defaultRule")}</option>
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
                        <div className="flex flex-col gap-1">
                          {(driverType === "mysql" || driverType === "mariadb" || driverType === "mssql" || driverType === "sqlserver") &&
                            <input className={inputCls} value={fk.refDatabase ?? ""} placeholder="引用数据库（可选）" disabled={fk.deleted}
                              onChange={(e) => updateFk({ refDatabase: e.target.value || undefined })} />}
                          {driverType !== "mysql" && driverType !== "mariadb" &&
                            <input className={inputCls} value={fk.refSchema ?? ""} placeholder="引用 schema（可选）" disabled={fk.deleted}
                              onChange={(e) => updateFk({ refSchema: e.target.value || undefined })} />}
                          <input className={inputCls} value={fk.refTable} placeholder={t("tableView.thRefTable")} disabled={fk.deleted}
                            onChange={(e) => updateFk({ refTable: e.target.value })} />
                        </div>
                      ) : (
                        [fk.refDatabase, fk.refSchema, fk.refTable].filter(Boolean).map((part) => quoteIdent(part!, driverType)).join(".")
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
                        title={fk.deleted ? t("tableView.restore") : t("tableView.deleteFk")}
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
        <Button variant="ghost" size="sm" className="h-6" onClick={() => void load(true)} disabled={loading || submitting}>
          {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
          <span className="ml-1">{t("common.refresh")}</span>
        </Button>
        <span className="text-muted-foreground/60">
          {[database, schema, table].filter(Boolean).join(" / ")}
        </span>
      </div>
    </div>
  );
}
