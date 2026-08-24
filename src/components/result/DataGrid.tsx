import { useEffect, useMemo, useState } from "react";
import { EditContext, QueryResult } from "@/types/database";
import { cn, quoteIdent, sqlLiteral } from "@/lib/utils";
import { Button } from "@/components/ui";
import { Plus, Check, RotateCcw, Loader2, Pencil, Trash2, Undo2, Copy } from "lucide-react";
import { useT } from "@/i18n";

interface DataGridProps {
  result: QueryResult | undefined;
  editContext?: EditContext | null;
  /** 提交批量 UPDATE/INSERT（后端事务执行），成功后父组件会刷新结果 */
  onApplyEdits?: (statements: string[]) => Promise<void>;
}

interface EditingCell {
  key: string;
  value: string;
}

export function DataGrid({ result, editContext, onApplyEdits }: DataGridProps) {
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

  // 结果集变化（重新执行）时清空所有未提交改动
  useEffect(() => {
    setEdits({});
    setNewRows([]);
    setDeletedRows(new Set());
    setEditingCell(null);
    setSubmitError(null);
  }, [result]);

  const editable = !!editContext && !!onApplyEdits;
  const hasPrimaryKey = (editContext?.primaryKeys.length ?? 0) > 0;

  /** 排序后的行 + 原始下标（编辑/WHERE 定位必须用原始下标） */
  const sortedRows = useMemo(() => {
    if (!result) return [];
    const indexed = result.rows.map((row, origIdx) => ({ row, origIdx }));
    if (!sortConfig) return indexed;
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
  }, [result, sortConfig]);

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
    const { table, primaryKeys, driverType } = editContext;
    const columns = result.columns;
    const statements: string[] = [];
    const q = (name: string) => quoteIdent(name, driverType);

    // 主键 WHERE 条件（用原值定位行）
    const pkWhere = (rowIdx: number) =>
      primaryKeys
        .map((pk) => {
          const pkCol = columns.findIndex((c) => c.name === pk);
          return `${q(pk)} = ${sqlLiteral(result.rows[rowIdx].values[pkCol])}`;
        })
        .join(" AND ");

    // UPDATE：按行分组（已标记删除的行跳过）
    const byRow = new Map<number, Map<number, string>>();
    for (const [key, value] of Object.entries(edits)) {
      const [r, c] = key.split(":").map(Number);
      if (deletedRows.has(r)) continue;
      if (!byRow.has(r)) byRow.set(r, new Map());
      byRow.get(r)!.set(c, value);
    }
    for (const [rowIdx, cols] of byRow) {
      const sets = [...cols.entries()].map(
        ([c, v]) => `${q(columns[c].name)} = ${sqlLiteral(v)}`
      );
      statements.push(`UPDATE ${q(table)} SET ${sets.join(", ")} WHERE ${pkWhere(rowIdx)};`);
    }

    // DELETE
    for (const rowIdx of deletedRows) {
      statements.push(`DELETE FROM ${q(table)} WHERE ${pkWhere(rowIdx)};`);
    }

    // INSERT：只带有值的列（缺省列走数据库默认值）
    for (const newRow of newRows) {
      const entries = Object.entries(newRow);
      if (entries.length === 0) continue;
      const colNames = entries.map(([c]) => q(columns[Number(c)].name));
      const vals = entries.map(([, v]) => sqlLiteral(v));
      statements.push(`INSERT INTO ${q(table)} (${colNames.join(", ")}) VALUES (${vals.join(", ")});`);
    }

    return statements;
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

  const cellBase = "px-3 py-1 border-b border-r whitespace-nowrap overflow-hidden text-ellipsis max-w-[300px]";

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

      {/* 数据表格 */}
      <div className="flex-1 overflow-auto custom-scrollbar">
        <table className="w-full text-sm border-collapse">
          <thead className="sticky top-0 z-10">
            <tr className="bg-gray-100 dark:bg-gray-800">
              <th className="px-2 py-1.5 text-left font-semibold text-xs border-b border-r w-12 text-center bg-gray-100 dark:bg-gray-800">
                #
              </th>
              {result.columns.map((col) => (
                <th
                  key={col.name}
                  className="px-3 py-1.5 text-left font-semibold text-xs border-b border-r cursor-pointer hover:bg-gray-200 dark:hover:bg-gray-700 whitespace-nowrap select-none bg-gray-100 dark:bg-gray-800"
                  onClick={() => {
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
                    {sortConfig?.key === col.name && (
                      <span>{sortConfig.direction === "asc" ? "▲" : "▼"}</span>
                    )}
                  </div>
                  <div className="text-[10px] text-gray-400 font-normal">{col.dataType}</div>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {sortedRows.map(({ row, origIdx }, rowIndex) => {
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
                    return (
                      <td
                        key={colIndex}
                        className={cn(
                          cellBase,
                          canEdit && !isDeleted && "cursor-cell",
                          edits[key] !== undefined && "bg-amber-50 dark:bg-amber-900/20"
                        )}
                        onDoubleClick={() => {
                          if (canEdit && !isDeleted)
                            setEditingCell({ key, value: edits[key] ?? original });
                        }}
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
                      onDoubleClick={() => setEditingCell({ key, value: current })}
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
    </div>
  );
}
