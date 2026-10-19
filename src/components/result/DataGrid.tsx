import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { EditContext, QueryResult } from "@/types/database";
import { cn } from "@/lib/utils";
import { buildEditStatements } from "@/lib/gridEdits";
import { Button } from "@/components/ui";
import { Plus, Check, RotateCcw, Loader2, Pencil, Trash2, Undo2, Copy } from "lucide-react";
import { useT } from "@/i18n";

/** 排序状态；分页场景由父组件转成 SQL ORDER BY 下推数据库 */
export interface GridSort {
  key: string;
  direction: "asc" | "desc";
}

interface DataGridProps {
  result: QueryResult | undefined;
  editContext?: EditContext | null;
  /** 提交批量 UPDATE/INSERT（后端事务执行），成功后父组件会刷新结果 */
  onApplyEdits?: (statements: string[]) => Promise<void>;
  /**
   * 受控排序：分页数据（表数据浏览）必须由父组件把 ORDER BY 拼进查询，
   * 内存排序只能作用于当前已拉取的页。传入 onSortChange 后本地排序禁用，
   * 表头点击回调 onSortChange，排序指示由 sortState 驱动。
   */
  sortState?: GridSort | null;
  onSortChange?: (sort: GridSort) => void;
}

interface EditingCell {
  key: string;
  value: string;
}

/** 虚拟滚动参数：行高固定（px-1 + leading-5 + border 1px = 29px），只渲染可视区 ± OVERSCAN 行 */
const ROW_HEIGHT = 29;
const OVERSCAN = 10;

export function DataGrid({ result, editContext, onApplyEdits, sortState, onSortChange }: DataGridProps) {
  const t = useT();
  const [sortConfig, setSortConfig] = useState<{ key: string; direction: "asc" | "desc" } | null>(null);
  /** 已有行的编辑：key = `${origRowIdx}:${colIdx}`，value = 新文本 */
  const [edits, setEdits] = useState<Record<string, string>>({});
  /** 新增行：每行是 colIdx -> 文本 */
  const [newRows, setNewRows] = useState<Record<number, string>[]>([]);
  /** 标记删除的行（原始下标） */
  const [deletedRows, setDeletedRows] = useState<Set<number>>(new Set());
  const [editingCell, setEditingCell] = useState<EditingCell | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  /** 会话内列宽调整（列名 -> 像素），查询列集合变化时重置 */
  const [colWidths, setColWidths] = useState<Record<string, number>>({});
  /** 悬停浮层：展示被截断单元格的完整内容 */
  const [cellTip, setCellTip] = useState<{ text: string; style: React.CSSProperties } | null>(null);
  const [cellTipCopied, setCellTipCopied] = useState(false);
  const tipShowTimer = useRef<number | null>(null);
  const tipHideTimer = useRef<number | null>(null);
  // 虚拟滚动：滚动位置与可视区高度
  const scrollRef = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewHeight, setViewHeight] = useState(0);

  const columnSignature = result?.columns.map((c) => c.name).join("\u0001") ?? "";

  // 结果集变化（重新执行）时清空所有未提交改动，回到顶部
  useEffect(() => {
    setEdits({});
    setNewRows([]);
    setDeletedRows(new Set());
    setEditingCell(null);
    setSubmitError(null);
    hideCellTip();
    scrollRef.current?.scrollTo({ top: 0 });
    setScrollTop(0);
  }, [result]);

  // 跟踪可视区尺寸（窗口/面板拖拽都会变化）。首次渲染尚无查询结果，
  // scrollRef 对应的容器还不存在；结果返回后必须重新绑定观察器，否则
  // viewHeight 会一直为 0，虚拟列表只能渲染 OVERSCAN 指定的 10 行。
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const update = () => setViewHeight(el.clientHeight);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, [result]);

  // 滚动：直接更新（React 批处理），单帧渲染仅 ~40 行，开销可忽略
  const handleGridScroll = useCallback(() => {
    hideCellTip();
    setScrollTop(scrollRef.current?.scrollTop ?? 0);
  }, []);

  useEffect(
    () => () => {
      if (tipShowTimer.current !== null) window.clearTimeout(tipShowTimer.current);
      if (tipHideTimer.current !== null) window.clearTimeout(tipHideTimer.current);
    },
    []
  );

  // 列集合变化（切换表/换查询）时重置列宽；重跑相同查询保留已调宽度
  useEffect(() => {
    setColWidths({});
  }, [columnSignature]);

  const editable = !!editContext && !!onApplyEdits;
  const hasPrimaryKey = (editContext?.primaryKeys.length ?? 0) > 0;

  /** 当前生效的排序：受控模式用父组件状态，否则用本地状态 */
  const activeSort = onSortChange ? sortState ?? null : sortConfig;

  /** 排序后的行 + 原始下标（编辑/WHERE 定位必须用原始下标） */
  const sortedRows = useMemo(() => {
    if (!result) return [];
    const indexed = result.rows.map((row, origIdx) => ({ row, origIdx }));
    // 受控排序（服务端 ORDER BY）下数据已是全量排序结果，本地不再重排
    if (onSortChange || !sortConfig) return indexed;
    const colIndex = result.columns.findIndex((c) => c.name === sortConfig.key);
    if (colIndex === -1) return indexed;

    return [...indexed].sort((a, b) => {
      const aVal = a.row.values[colIndex];
      const bVal = b.row.values[colIndex];
      if (aVal == null && bVal == null) return 0;
      if (aVal == null) return 1;
      if (bVal == null) return -1;
      if (typeof aVal === "number" && typeof bVal === "number") {
        return sortConfig.direction === "asc" ? aVal - bVal : bVal - aVal;
      }
      const aStr = String(aVal);
      const bStr = String(bVal);
      return sortConfig.direction === "asc" ? aStr.localeCompare(bStr) : bStr.localeCompare(aStr);
    });
  }, [result, sortConfig, onSortChange]);

  const pendingCount =
    Object.keys(edits).length +
    newRows.filter((r) => Object.keys(r).length > 0).length +
    deletedRows.size;

  const commitEdit = (key: string, value: string, original: string) => {
    setEdits((prev) => {
      const next = { ...prev };
      if (value === original) delete next[key];
      else next[key] = value;
      return next;
    });
    setEditingCell(null);
  };

  const commitNewRowCell = (rowIdx: number, colIdx: number, value: string) => {
    setNewRows((prev) => {
      const next = [...prev];
      const row = { ...next[rowIdx] };
      if (value === "") delete row[colIdx];
      else row[colIdx] = value;
      next[rowIdx] = row;
      return next;
    });
    setEditingCell(null);
  };

  /** 生成批量 UPDATE / INSERT / DELETE 语句 */
  const buildStatements = (): string[] => {
    if (!result || !editContext) return [];
    return buildEditStatements(result, editContext, edits, newRows, deletedRows);
  };

  const handleSubmit = async () => {
    if (!onApplyEdits) return;
    const statements = buildStatements();
    if (statements.length === 0) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      await onApplyEdits(statements);
      // 父组件会刷新结果，useEffect 负责清空本地状态
    } catch (err) {
      setSubmitError(String(err));
    } finally {
      setSubmitting(false);
    }
  };

  const handleDiscard = () => {
    setEdits({});
    setNewRows([]);
    setDeletedRows(new Set());
    setEditingCell(null);
    setSubmitError(null);
  };

  /** 复制全部结果为 TSV（无表头，方便直接粘贴到编辑器执行/粘贴到 Excel） */
  const handleCopyAll = async () => {
    if (!result) return;
    const lines = result.rows.map((r) =>
      r.values
        .map((v) =>
          v === null || v === undefined
            ? "NULL"
            : String(v).replace(/\t/g, " ").replace(/\r?\n/g, "\\n")
        )
        .join("\t")
    );
    try {
      await navigator.clipboard.writeText(lines.join("\n"));
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch (err) {
      setSubmitError(t("grid.copyFailed", { error: String(err) }));
    }
  };

  const toggleDeleteRow = (origIdx: number) => {
    setDeletedRows((prev) => {
      const next = new Set(prev);
      if (next.has(origIdx)) next.delete(origIdx);
      else next.add(origIdx);
      return next;
    });
  };

  /** 拖拽表头右缘调整列宽（会话内有效） */
  const startColumnResize = (e: React.MouseEvent, columnName: string) => {
    e.preventDefault();
    e.stopPropagation();
    const th = (e.currentTarget as HTMLElement).closest("th") as HTMLTableCellElement | null;
    const startX = e.clientX;
    const startWidth = th?.offsetWidth ?? 200;

    const onMove = (ev: MouseEvent) => {
      const width = Math.round(startWidth + ev.clientX - startX);
      setColWidths((prev) => ({ ...prev, [columnName]: Math.min(1000, Math.max(60, width)) }));
    };
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  };

  const resetColumnWidth = (columnName: string) => {
    setColWidths((prev) => {
      if (!(columnName in prev)) return prev;
      const next = { ...prev };
      delete next[columnName];
      return next;
    });
  };

  /** 已调宽度的列用内联样式钉住；未调整的列保持内容自适应（最多 300px 截断） */
  const columnStyle = (columnName: string): React.CSSProperties => {
    const width = colWidths[columnName];
    return width ? { width, minWidth: width, maxWidth: width } : {};
  };

  const hideCellTip = () => {
    if (tipShowTimer.current !== null) {
      window.clearTimeout(tipShowTimer.current);
      tipShowTimer.current = null;
    }
    if (tipHideTimer.current !== null) {
      window.clearTimeout(tipHideTimer.current);
      tipHideTimer.current = null;
    }
    setCellTip(null);
    setCellTipCopied(false);
  };

  /** 移出单元格后延迟隐藏，给鼠标移入浮层点"复制"留出时间 */
  const hideCellTipSoon = () => {
    if (tipShowTimer.current !== null) {
      window.clearTimeout(tipShowTimer.current);
      tipShowTimer.current = null;
    }
    if (tipHideTimer.current !== null) window.clearTimeout(tipHideTimer.current);
    tipHideTimer.current = window.setTimeout(() => {
      tipHideTimer.current = null;
      setCellTip(null);
      setCellTipCopied(false);
    }, 300);
  };

  /**
   * 悬停 500ms 后，若内容确实被截断则显示浮层。
   * 已显示的浮层不会因为路过其他单元格而立即消失——否则鼠标移向浮层的
   * 路径会经过下方单元格，浮层在进入前就被清掉，永远点不到复制按钮。
   */
  const scheduleCellTip = (el: HTMLTableCellElement, text: string) => {
    // 在单元格间移动时保持连续性：取消"延迟隐藏"，重新计时显示
    if (tipHideTimer.current !== null) {
      window.clearTimeout(tipHideTimer.current);
      tipHideTimer.current = null;
    }
    if (tipShowTimer.current !== null) window.clearTimeout(tipShowTimer.current);
    tipShowTimer.current = window.setTimeout(() => {
      tipShowTimer.current = null;
      // 内容没有溢出、或正在单元格内编辑时不显示，并收起旧浮层
      if (el.scrollWidth <= el.clientWidth + 1 || el.querySelector("input")) {
        setCellTip(null);
        return;
      }
      const rect = el.getBoundingClientRect();
      const width = Math.min(480, Math.max(200, Math.ceil(rect.width)));
      const left = Math.min(Math.max(8, rect.left), window.innerWidth - width - 8);
      const style: React.CSSProperties =
        window.innerHeight - rect.bottom >= 96
          ? { left, top: rect.bottom + 6 }
          : { left, bottom: window.innerHeight - rect.top + 6 };
      setCellTip({ text, style });
    }, 500);
  };

  const handleCopyCellTip = async () => {
    if (!cellTip) return;
    try {
      await navigator.clipboard.writeText(cellTip.text);
      setCellTipCopied(true);
      window.setTimeout(() => setCellTipCopied(false), 1500);
    } catch (err) {
      setSubmitError(t("grid.copyFailed", { error: String(err) }));
    }
  };

  if (!result) {
    return (
      <div className="flex items-center justify-center h-full text-muted-foreground text-sm">
        <p>{t("grid.runToSee")}</p>
      </div>
    );
  }

  if (result.error) {
    return (
      <div className="flex items-center justify-center h-full text-sm px-4">
        <div className="text-center max-w-full">
          <p className="text-red-600 font-medium">{t("grid.execFailed")}</p>
          <p className="text-red-500 mt-1 break-words whitespace-pre-wrap font-mono text-xs">
            {result.error}
          </p>
        </div>
      </div>
    );
  }

  if (!result.columns.length) {
    return (
      <div className="flex items-center justify-center h-full text-sm">
        <div className="text-center">
          <p className="text-green-600 font-medium">{t("grid.execSuccess")}</p>
          <p className="text-muted-foreground mt-1">
            {t("grid.affected", { count: result.rowsAffected || 0, ms: result.executionTimeMs })}
          </p>
        </div>
      </div>
    );
  }

  /** 渲染单元格内容（含编辑态覆盖） */
  const renderCellValue = (origIdx: number, colIdx: number, value: unknown) => {
    const key = `${origIdx}:${colIdx}`;
    const edited = edits[key];
    const display = edited !== undefined ? edited : value;

    return (
      <>
        {display === null || display === undefined ? (
          <span className="text-gray-400 italic">NULL</span>
        ) : typeof display === "boolean" ? (
          <span className={display ? "text-green-600" : "text-red-600"}>
            {display ? "true" : "false"}
          </span>
        ) : (
          String(display)
        )}
        {edited !== undefined && <Pencil className="inline h-3 w-3 ml-1 text-amber-500" />}
      </>
    );
  };

  /** 编辑中的单元格 input */
  const renderCellInput = (
    key: string,
    initial: string,
    onCommit: (value: string) => void
  ) => (
    <input
      autoFocus
      className="w-full px-1 py-0.5 text-sm border border-blue-400 rounded outline-none bg-white dark:bg-gray-900"
      value={editingCell?.value ?? initial}
      onChange={(e) => setEditingCell({ key, value: e.target.value })}
      onBlur={(e) => onCommit(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Enter") onCommit((e.target as HTMLInputElement).value);
        if (e.key === "Escape") setEditingCell(null);
      }}
    />
  );

  // h-7 + leading-5 把行高钉在 28px（= ROW_HEIGHT），虚拟滚动的占位计算依赖这一点
  const cellBase =
    "px-3 py-1 h-7 leading-5 border-b border-r whitespace-nowrap overflow-hidden text-ellipsis max-w-[300px]";

  // 虚拟滚动：只渲染可视区附近的行，上下用占位行撑起总高度
  const totalRows = sortedRows.length;
  const startIdx = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const endIdx = Math.min(totalRows, Math.ceil((scrollTop + viewHeight) / ROW_HEIGHT) + OVERSCAN);
  const visibleRows = sortedRows.slice(startIdx, endIdx);
  const spacerColSpan = result.columns.length + 1;

  return (
    <div className="flex flex-col h-full">
      {/* 结果信息 + 编辑操作栏 */}
      <div className="flex items-center justify-between px-3 py-1.5 border-b bg-muted/20 text-xs gap-2">
        <span className="text-muted-foreground">
          {t("grid.rows", { count: result.rows.length.toLocaleString() })}
          {result.truncated && (
            <span className="text-amber-600 font-medium">{t("grid.truncatedWarning")}</span>
          )}
          {result.executionTimeMs > 0 ? ` · ${result.executionTimeMs}ms` : ""}
          {editContext && (
            <span className="ml-2 text-blue-600">{t("grid.tablePrefix", { table: editContext.table })}</span>
          )}
          {editContext && !hasPrimaryKey && (
            <span className="ml-2 text-amber-600">{t("grid.noPrimaryKey")}</span>
          )}
        </span>

        <div className="flex items-center gap-2">
          {editable && hasPrimaryKey && (
            <Button
              variant="ghost"
              size="sm"
              className="h-6 text-xs"
              onClick={() => setNewRows((prev) => [...prev, {}])}
            >
              <Plus className="h-3 w-3 mr-1" />
              {t("grid.addRow")}
            </Button>
          )}
          {pendingCount > 0 && (
            <>
              <span className="text-amber-600 font-medium">{t("grid.pending", { count: pendingCount })}</span>
              <Button
                size="sm"
                className="h-6 text-xs"
                onClick={handleSubmit}
                disabled={submitting}
              >
                {submitting ? <Loader2 className="h-3 w-3 animate-spin" /> : <Check className="h-3 w-3 mr-1" />}
                {t("grid.submit")}
              </Button>
              <Button
                variant="outline"
                size="sm"
                className="h-6 text-xs"
                onClick={handleDiscard}
                disabled={submitting}
              >
                <RotateCcw className="h-3 w-3 mr-1" />
                {t("grid.discard")}
              </Button>
            </>
          )}
          <Button
            variant="ghost"
            size="sm"
            className="h-6 text-xs"
            title={t("grid.copyAllTitle")}
            onClick={handleCopyAll}
          >
            {copied ? <Check className="h-3 w-3 mr-1 text-green-600" /> : <Copy className="h-3 w-3 mr-1" />}
            {copied ? t("grid.copied") : t("grid.copyAll")}
          </Button>
          <span className="text-muted-foreground">{t("grid.columns", { count: result.columns.length })}</span>
        </div>
      </div>

      {submitError && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">
          {t("grid.submitFailed", { error: submitError })}
        </div>
      )}

      {/* 数据表格；overflow-anchor:none 防止浏览器滚动锚定与占位行变化互相干扰 */}
      <div
        ref={scrollRef}
        className="flex-1 overflow-auto custom-scrollbar [overflow-anchor:none]"
        onScroll={handleGridScroll}
      >
        <table className="w-full text-sm border-collapse">
          <thead className="sticky top-0 z-10">
            <tr className="bg-gray-100 dark:bg-gray-800">
              <th className="px-2 py-1.5 text-left font-semibold text-xs border-b border-r w-12 text-center bg-gray-100 dark:bg-gray-800">
                #
              </th>
              {result.columns.map((col) => (
                <th
                  key={col.name}
                  className="relative overflow-hidden px-3 py-1.5 text-left font-semibold text-xs border-b border-r cursor-pointer hover:bg-gray-200 dark:hover:bg-gray-700 whitespace-nowrap select-none bg-gray-100 dark:bg-gray-800"
                  style={columnStyle(col.name)}
                  onClick={() => {
                    if (onSortChange) {
                      // 分页浏览：排序交给父组件重查（ORDER BY 下推数据库）
                      onSortChange({
                        key: col.name,
                        direction:
                          activeSort?.key === col.name && activeSort.direction === "asc"
                            ? "desc"
                            : "asc",
                      });
                      return;
                    }
                    setSortConfig((prev) =>
                      prev?.key === col.name
                        ? { key: col.name, direction: prev.direction === "asc" ? "desc" : "asc" }
                        : { key: col.name, direction: "asc" }
                    );
                  }}
                >
                  <div className="flex items-center gap-1">
                    {col.name}
                    {editContext?.primaryKeys.includes(col.name) && (
                      <span className="text-yellow-500" title={t("grid.primaryKey")}>🔑</span>
                    )}
                    {activeSort?.key === col.name && (
                      <span>{activeSort.direction === "asc" ? "▲" : "▼"}</span>
                    )}
                  </div>
                  <div className="text-[10px] text-gray-400 font-normal">{col.dataType}</div>
                  <span
                    title={t("grid.resizeColumn")}
                    className="absolute inset-y-0 right-0 z-10 w-1.5 cursor-col-resize hover:bg-blue-400/60"
                    onMouseDown={(e) => startColumnResize(e, col.name)}
                    onClick={(e) => e.stopPropagation()}
                    onDoubleClick={(e) => {
                      e.stopPropagation();
                      resetColumnWidth(col.name);
                    }}
                  />
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {startIdx > 0 && (
              <tr aria-hidden>
                <td
                  style={{ height: startIdx * ROW_HEIGHT, padding: 0, border: "none" }}
                  colSpan={spacerColSpan}
                />
              </tr>
            )}
            {visibleRows.map(({ row, origIdx }, i) => {
              const rowIndex = startIdx + i;
              const isDeleted = deletedRows.has(origIdx);
              const canEdit = editable && hasPrimaryKey;
              return (
                <tr
                  key={origIdx}
                  className={cn(
                    "group/row row-hover",
                    rowIndex % 2 === 0 ? "bg-white dark:bg-gray-900" : "bg-gray-50/50 dark:bg-gray-800/30",
                    isDeleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400"
                  )}
                >
                  <td className="px-2 py-1 border-b border-r text-center text-xs text-gray-400 w-12">
                    {canEdit ? (
                      <span className="relative inline-block w-6">
                        <span className={cn("group-hover/row:invisible", isDeleted && "invisible")}>
                          {rowIndex + 1}
                        </span>
                        <button
                          title={isDeleted ? t("grid.restoreRow") : t("grid.deleteRow")}
                          className={cn(
                            "absolute inset-0 items-center justify-center",
                            isDeleted ? "flex" : "hidden group-hover/row:flex"
                          )}
                          onClick={() => toggleDeleteRow(origIdx)}
                        >
                          {isDeleted ? (
                            <Undo2 className="h-3 w-3 text-green-600" />
                          ) : (
                            <Trash2 className="h-3 w-3 text-red-500" />
                          )}
                        </button>
                      </span>
                    ) : (
                      rowIndex + 1
                    )}
                  </td>
                  {row.values.map((value, colIndex) => {
                    const key = `${origIdx}:${colIndex}`;
                    const original = value === null || value === undefined ? "NULL" : String(value);
                    const edited = edits[key];
                    return (
                      <td
                        key={colIndex}
                        className={cn(
                          cellBase,
                          canEdit && !isDeleted && "cursor-cell",
                          edited !== undefined && "bg-amber-50 dark:bg-amber-900/20"
                        )}
                        style={columnStyle(result.columns[colIndex]?.name ?? "")}
                        onDoubleClick={() => {
                          if (canEdit && !isDeleted) {
                            hideCellTip();
                            setEditingCell({ key, value: edited ?? original });
                          }
                        }}
                        onMouseEnter={(e) => {
                          if (editingCell?.key !== key) {
                            scheduleCellTip(e.currentTarget, edited ?? original);
                          }
                        }}
                        onMouseLeave={hideCellTipSoon}
                      >
                        {editingCell?.key === key
                          ? renderCellInput(key, original, (v) => commitEdit(key, v, original))
                          : renderCellValue(origIdx, colIndex, value)}
                      </td>
                    );
                  })}
                </tr>
              );
            })}

            {endIdx < totalRows && (
              <tr aria-hidden>
                <td
                  style={{ height: (totalRows - endIdx) * ROW_HEIGHT, padding: 0, border: "none" }}
                  colSpan={spacerColSpan}
                />
              </tr>
            )}

            {/* 新增行 */}
            {newRows.map((newRow, newIdx) => (
              <tr key={`new-${newIdx}`} className="bg-green-50/60 dark:bg-green-900/10">
                <td className="px-2 py-1 border-b border-r text-center text-xs text-green-600">
                  {t("grid.newBadge")}
                </td>
                {result.columns.map((_, colIndex) => {
                  const key = `new:${newIdx}:${colIndex}`;
                  const current = newRow[colIndex] ?? "";
                  return (
                    <td
                      key={colIndex}
                      className={cn(cellBase, "cursor-cell")}
                      style={columnStyle(result.columns[colIndex]?.name ?? "")}
                      onDoubleClick={() => {
                        hideCellTip();
                        setEditingCell({ key, value: current });
                      }}
                      onMouseEnter={(e) => {
                        if (editingCell?.key !== key && current) {
                          scheduleCellTip(e.currentTarget, current);
                        }
                      }}
                      onMouseLeave={hideCellTipSoon}
                    >
                      {editingCell?.key === key
                        ? renderCellInput(key, current, (v) => commitNewRowCell(newIdx, colIndex, v))
                        : current || <span className="text-gray-300 italic">{t("grid.doubleClickEdit")}</span>}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {/* 悬停浮层：显示被截断单元格的完整内容（挂到 body，避免被表格裁剪） */}
      {cellTip &&
        createPortal(
          <div
            className="fixed z-50 min-w-[180px] max-w-[480px] rounded-md border border-gray-200 bg-white shadow-lg dark:border-gray-600 dark:bg-gray-800"
            style={cellTip.style}
            onMouseEnter={() => {
              if (tipHideTimer.current !== null) {
                window.clearTimeout(tipHideTimer.current);
                tipHideTimer.current = null;
              }
            }}
            onMouseLeave={hideCellTip}
          >
            <button
              title={t("grid.copyCell")}
              className="absolute right-1 top-1 rounded p-1 text-gray-400 hover:bg-gray-100 hover:text-gray-600 dark:hover:bg-gray-700 dark:hover:text-gray-300"
              onClick={handleCopyCellTip}
            >
              {cellTipCopied ? (
                <Check className="h-3 w-3 text-green-600" />
              ) : (
                <Copy className="h-3 w-3" />
              )}
            </button>
            <div className="custom-scrollbar max-h-[320px] overflow-auto whitespace-pre-wrap break-all px-2.5 py-1.5 pr-8">
              {cellTip.text}
            </div>
          </div>,
          document.body
        )}
    </div>
  );
}
