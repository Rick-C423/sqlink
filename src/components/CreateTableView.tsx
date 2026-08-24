import { useState } from "react";
import { tauriService } from "@/services/tauriService";
import { buildCreateTableStatements, ColumnDraft, DefaultKind, FkDraft, IndexDraft } from "@/lib/ddl";
import { useConnectionStore } from "@/stores/connectionStore";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { Button } from "@/components/ui";
import { cn, commonDataTypes } from "@/lib/utils";
import { Check, Loader2, Plus, Undo2, X } from "lucide-react";
import { useT } from "@/i18n";

interface CreateTableViewProps {
  connectionId: string;
  database: string;
  /** workspace tab id，建表成功后关闭自己 */
  tabId: string;
}

type StructTab = "columns" | "indexes" | "fks";

/** 新建表设计器：列 / 索引 / 外键 + SQL 预览，提交后刷新连接树并打开新表 */
export function CreateTableView({ connectionId, database, tabId }: CreateTableViewProps) {
  const t = useT();
  const driverType =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.driverType ?? "mysql";
  const isMysql = driverType === "mysql";
  const dataTypes = commonDataTypes(driverType);

  const [tableName, setTableName] = useState("");
  const [tableComment, setTableComment] = useState("");
  const [structTab, setStructTab] = useState<StructTab>("columns");
  const [cols, setCols] = useState<ColumnDraft[]>([
    {
      name: "id",
      dataType: isMysql ? "INT" : "INTEGER",
      nullable: false,
      defaultKind: "null",
      defaultValue: "",
      isPrimary: true,
      autoIncrement: isMysql,
      comment: "",
      isNew: true,
    },
  ]);
  const [idxs, setIdxs] = useState<IndexDraft[]>([]);
  const [fks, setFks] = useState<FkDraft[]>([]);
  const [showSql, setShowSql] = useState(true);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const liveCols = cols.filter((c) => !c.deleted && c.name.trim());
  const statements = buildCreateTableStatements(driverType, tableName, cols, idxs, fks, tableComment);

  const updateCol = (i: number, patch: Partial<ColumnDraft>) =>
    setCols((prev) => prev.map((c, idx) => (idx === i ? { ...c, ...patch } : c)));

  const validate = (): string | null => {
    if (!tableName.trim()) return t("createTable.nameRequired");
    if (liveCols.length === 0) return t("createTable.needColumn");
    const names = liveCols.map((c) => c.name.trim().toLowerCase());
    if (new Set(names).size !== names.length) return t("createTable.dupColumn");
    return null;
  };

  const handleSubmit = async () => {
    const err = validate();
    if (err) {
      setError(err);
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await tauriService.executeBatch(connectionId, database, statements);

      // 刷新连接树中该库的表列表
      const dbNodeId = `${connectionId}::db::${database}`;
      const tables = await tauriService.getTables(connectionId, database);
      const cs = useConnectionStore.getState();
      cs.updateNodeChildren(
        dbNodeId,
        tables.map((tb) => ({
          id: `${dbNodeId}::table::${tb.name}`,
          type: "table" as const,
          name: tb.name,
          metadata: { rowCount: tb.rowCount },
        }))
      );
      cs.expandNode(dbNodeId);

      // 打开新表视图并关闭设计器
      const ws = useWorkspaceStore.getState();
      ws.openTable(connectionId, database, tableName.trim());
      ws.close(tabId);
    } catch (err) {
      setError(t("createTable.createFailed", { error: String(err) }));
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
      <div className="flex items-center gap-2 px-3 py-1.5 border-b text-xs flex-wrap">
        <input
          className={cn(inputCls, "w-44 h-7")}
          value={tableName}
          placeholder={t("createTable.tableNamePlaceholder")}
          autoFocus
          onChange={(e) => setTableName(e.target.value)}
        />
        <input
          className={cn(inputCls, "w-36 h-7")}
          value={tableComment}
          placeholder={t("createTable.commentPlaceholder")}
          onChange={(e) => setTableComment(e.target.value)}
        />
        <span className="text-muted-foreground">{t("createTable.createIn", { database })}</span>

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
            {st === "columns"
              ? t("createTable.columnsTab", { count: liveCols.length })
              : st === "indexes"
                ? t("createTable.indexesTab", { count: idxs.filter((i) => !i.deleted).length })
                : t("createTable.fksTab", { count: fks.filter((f) => !f.deleted).length })}
          </button>
        ))}

        <div className="ml-auto flex items-center gap-1.5">
          <Button variant="ghost" size="sm" className="h-6 text-xs" onClick={() => setShowSql(!showSql)}>
            {showSql ? t("tableView.hideSql") : t("tableView.previewSql")}
          </Button>
          <Button size="sm" className="h-6 text-xs" onClick={handleSubmit} disabled={submitting || statements.length === 0}>
            {submitting ? <Loader2 className="h-3 w-3 animate-spin" /> : <Check className="h-3 w-3 mr-1" />}
            {t("createTable.createTable")}
          </Button>
        </div>
      </div>

      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200">{error}</div>
      )}
      {showSql && (
        <pre className="px-3 py-2 text-xs bg-gray-900 text-green-300 border-b overflow-auto max-h-40">
          {statements.length > 0 ? statements.join("\n") : t("createTable.sqlPlaceholder")}
        </pre>
      )}

      {structTab === "columns" ? (
        <div className="flex-1 overflow-auto custom-scrollbar">
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
              {cols.map((c, i) => (
                <tr
                  key={i}
                  className={cn(c.deleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400")}
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
                      list="datatype-list-create"
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
                    <button title={c.deleted ? t("tableView.restore") : t("tableView.deleteColumn")} onClick={() => updateCol(i, { deleted: !c.deleted })}>
                      {c.deleted ? (
                        <Undo2 className="h-3.5 w-3.5 text-green-600 inline" />
                      ) : (
                        <X className="h-3.5 w-3.5 text-gray-400 hover:text-red-600 inline" />
                      )}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : structTab === "indexes" ? (
        <div className="flex-1 overflow-auto custom-scrollbar">
          <div className="flex items-center justify-between px-3 py-2">
            <span className="text-xs font-semibold text-muted-foreground">{t("createTable.indexNote")}</span>
            <Button
              variant="ghost"
              size="sm"
              className="h-6 text-xs"
              onClick={() => setIdxs((prev) => [...prev, { name: "", columns: "", isUnique: false, isNew: true }])}
            >
              <Plus className="h-3 w-3 mr-1" />
              {t("tableView.addIndex")}
            </Button>
          </div>
          <table className="w-full text-sm border-collapse">
            <thead className="sticky top-0 z-10">
              <tr>
                <th className={thCls}>{t("createTable.indexNameAuto")}</th>
                <th className={thCls}>{t("tableView.thIndexCols")}</th>
                <th className={cn(thCls, "text-center w-14")}>{t("tableView.thUnique")}</th>
                <th className={cn(thCls, "text-center w-12")}></th>
              </tr>
            </thead>
            <tbody>
              {idxs.length === 0 && (
                <tr>
                  <td colSpan={4} className="px-3 py-4 text-center text-xs text-muted-foreground">
                    {t("createTable.noIndexes")}
                  </td>
                </tr>
              )}
              {idxs.map((idx, i) => (
                <tr key={i} className={cn(idx.deleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400")}>
                  <td className={tdCls}>
                    <input
                      className={inputCls}
                      value={idx.name}
                      placeholder="idx_xxx"
                      disabled={idx.deleted}
                      onChange={(e) =>
                        setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))
                      }
                    />
                  </td>
                  <td className={tdCls}>
                    <input
                      className={inputCls}
                      value={idx.columns}
                      placeholder="col1, col2"
                      disabled={idx.deleted}
                      onChange={(e) =>
                        setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, columns: e.target.value } : x)))
                      }
                    />
                  </td>
                  <td className={cn(tdCls, "text-center")}>
                    <input
                      type="checkbox"
                      checked={idx.isUnique}
                      disabled={idx.deleted}
                      onChange={(e) =>
                        setIdxs((prev) => prev.map((x, j) => (j === i ? { ...x, isUnique: e.target.checked } : x)))
                      }
                    />
                  </td>
                  <td className={cn(tdCls, "text-center")}>
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
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="flex-1 overflow-auto custom-scrollbar">
          <div className="flex items-center justify-between px-3 py-2">
            <span className="text-xs font-semibold text-muted-foreground">{t("tableView.fks")}</span>
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
                <th className={thCls}>{t("createTable.constraintOptional")}</th>
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
                    disabled={fk.deleted}
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
                  <tr key={i} className={cn(fk.deleted && "bg-red-50 dark:bg-red-900/20 line-through text-gray-400")}>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={fk.name}
                        placeholder="fk_xxx"
                        disabled={fk.deleted}
                        onChange={(e) => updateFk({ name: e.target.value })}
                      />
                    </td>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={fk.columns}
                        placeholder="col1, col2"
                        disabled={fk.deleted}
                        onChange={(e) => updateFk({ columns: e.target.value })}
                      />
                    </td>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={fk.refTable}
                        placeholder={t("tableView.thRefTable")}
                        disabled={fk.deleted}
                        onChange={(e) => updateFk({ refTable: e.target.value })}
                      />
                    </td>
                    <td className={tdCls}>
                      <input
                        className={inputCls}
                        value={fk.refColumns}
                        placeholder="id"
                        disabled={fk.deleted}
                        onChange={(e) => updateFk({ refColumns: e.target.value })}
                      />
                    </td>
                    <td className={tdCls}>{ruleSelect(fk.onDelete, (v) => updateFk({ onDelete: v }))}</td>
                    <td className={tdCls}>{ruleSelect(fk.onUpdate, (v) => updateFk({ onUpdate: v }))}</td>
                    <td className={cn(tdCls, "text-center")}>
                      <button title={fk.deleted ? t("tableView.restore") : t("tableView.deleteFk")} onClick={() => updateFk({ deleted: !fk.deleted })}>
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

      {/* 常用类型下拉建议（仍可自由输入） */}
      <datalist id="datatype-list-create">
        {dataTypes.map((dt) => (
          <option key={dt} value={dt} />
        ))}
      </datalist>
    </div>
  );
}
