import { useCallback, useEffect, useMemo, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { Button } from "@/components/ui";
import { cn } from "@/lib/utils";
import { Check, Loader2, Plus, Trash2 } from "lucide-react";
import { useT } from "@/i18n";

/**
 * 可视化权限编辑器（MySQL / MariaDB）：
 * 解析 SHOW GRANTS → 全局 / 分库两组复选框，保存时 diff 生成 GRANT / REVOKE。
 * 未解析的授权行（如 PROXY）原样保留，不做任何改动。
 */

/** 编辑器可管理的权限清单（分组展示）；ALL PRIVILEGES 展开为这整组 */
const PRIV_GROUPS: { key: "priv.groupData" | "priv.groupStructure" | "priv.groupAdmin"; privs: string[] }[] = [
  { key: "priv.groupData", privs: ["SELECT", "INSERT", "UPDATE", "DELETE"] },
  {
    key: "priv.groupStructure",
    privs: [
      "CREATE", "DROP", "ALTER", "INDEX", "REFERENCES",
      "CREATE VIEW", "SHOW VIEW", "TRIGGER", "EVENT",
      "CREATE TEMPORARY TABLES", "LOCK TABLES", "EXECUTE",
    ],
  },
  {
    key: "priv.groupAdmin",
    privs: ["RELOAD", "PROCESS", "REPLICATION CLIENT", "REPLICATION SLAVE", "CREATE USER"],
  },
];
const ALL_PRIVS = PRIV_GROUPS.flatMap((g) => g.privs);

/** 单引号转义，防注入 */
const sqlStr = (s: string) => s.replace(/'/g, "''");

interface ScopeGrants {
  /** "*.*" 或 "db.*" */
  scope: string;
  dbName: string | null;
  privs: Set<string>;
  grantOption: boolean;
}

interface ParsedGrants {
  scopes: ScopeGrants[];
  /** 无法解析的原始授权行（PROXY 等），保存时不触碰 */
  unparsed: string[];
}

function parseGrants(lines: string[]): ParsedGrants {
  const map = new Map<string, ScopeGrants>();
  const unparsed: string[] = [];

  for (const line of lines) {
    const m = line.match(/^GRANT\s+(.+?)\s+ON\s+(.+?)\s+TO\s+/i);
    if (!m) {
      unparsed.push(line);
      continue;
    }
    const privPart = m[1].toUpperCase();
    const scopeRaw = m[2].trim();
    const grantOption = /WITH\s+GRANT\s+OPTION/i.test(line);

    // 作用域：*.* 全局，否则 `db`.* / db.*
    let dbName: string | null = null;
    let scope = "*.*";
    if (scopeRaw !== "*.*") {
      const dm = scopeRaw.match(/^`?([^`]+)`?\.\*$/);
      if (!dm) {
        unparsed.push(line);
        continue;
      }
      dbName = dm[1];
      scope = `${dbName}.*`;
    }

    const privs = new Set<string>();
    if (/ALL\s+PRIVILEGES/.test(privPart)) {
      ALL_PRIVS.forEach((p) => privs.add(p));
    } else {
      privPart
        .split(",")
        .map((s) => s.trim())
        .filter((p) => ALL_PRIVS.includes(p))
        .forEach((p) => privs.add(p));
    }

    const existing = map.get(scope);
    if (existing) {
      privs.forEach((p) => existing.privs.add(p));
      existing.grantOption = existing.grantOption || grantOption;
    } else {
      map.set(scope, { scope, dbName, privs, grantOption });
    }
  }

  // 全局排最前，其余按库名排序
  const scopes = [...map.values()].sort((a, b) =>
    a.scope === "*.*" ? -1 : b.scope === "*.*" ? 1 : a.scope.localeCompare(b.scope)
  );
  return { scopes, unparsed };
}

const scopeSql = (s: ScopeGrants) =>
  s.scope === "*.*" ? "*.*" : `\`${(s.dbName ?? "").replace(/`/g, "``")}\`.*`;

/** 新旧权限 diff → GRANT / REVOKE 语句列表 */
function buildStatements(account: string, oldScopes: ScopeGrants[], newScopes: ScopeGrants[]): string[] {
  const stmts: string[] = [];
  const oldMap = new Map(oldScopes.map((s) => [s.scope, s]));
  const newMap = new Map(newScopes.map((s) => [s.scope, s]));

  for (const key of new Set([...oldMap.keys(), ...newMap.keys()])) {
    const o = oldMap.get(key);
    const n = newMap.get(key);
    const scope = scopeSql(n ?? o!);
    const oPrivs = o?.privs ?? new Set<string>();
    const nPrivs = n?.privs ?? new Set<string>();

    // 整个作用域被清空/移除：REVOKE ALL
    if (o && (!n || nPrivs.size === 0) && oPrivs.size > 0) {
      stmts.push(`REVOKE ALL PRIVILEGES ON ${scope} FROM ${account};`);
      if (o.grantOption) stmts.push(`REVOKE GRANT OPTION ON ${scope} FROM ${account};`);
      continue;
    }
    if (!n || nPrivs.size === 0) continue;

    const toAdd = [...nPrivs].filter((p) => !oPrivs.has(p));
    const toRemove = [...oPrivs].filter((p) => !nPrivs.has(p));
    const oGO = o?.grantOption ?? false;

    if (toRemove.length > 0) {
      stmts.push(`REVOKE ${toRemove.join(", ")} ON ${scope} FROM ${account};`);
    }
    if (toAdd.length > 0) {
      stmts.push(
        `GRANT ${toAdd.join(", ")} ON ${scope} TO ${account}${n.grantOption && !oGO ? " WITH GRANT OPTION" : ""};`
      );
    } else if (n.grantOption && !oGO) {
      // 只加了 GRANT OPTION：重复授权现有权限并附带 WITH GRANT OPTION（幂等）
      stmts.push(`GRANT ${[...nPrivs].join(", ")} ON ${scope} TO ${account} WITH GRANT OPTION;`);
    }
    if (!n.grantOption && oGO) {
      stmts.push(`REVOKE GRANT OPTION ON ${scope} FROM ${account};`);
    }
  }
  return stmts;
}

interface PrivilegeEditorProps {
  connectionId: string;
  name: string;
  host?: string;
  /** 保存成功（或取消）后回调，父组件负责刷新并收起 */
  onSaved: () => void;
}

export function PrivilegeEditor({ connectionId, name, host, onSaved }: PrivilegeEditorProps) {
  const t = useT();
  const account = `'${sqlStr(name)}'@'${sqlStr(host ?? "%")}'`;

  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [original, setOriginal] = useState<ScopeGrants[]>([]);
  const [draft, setDraft] = useState<ScopeGrants[]>([]);
  const [unparsed, setUnparsed] = useState<string[]>([]);
  const [databases, setDatabases] = useState<string[]>([]);
  const [addDbValue, setAddDbValue] = useState("");
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const [grantsResult, dbs] = await Promise.all([
        tauriService.executeQuery(connectionId, `SHOW GRANTS FOR ${account}`),
        tauriService.getDatabases(connectionId).catch(() => [] as string[]),
      ]);
      const lines = grantsResult.rows.map((r) => String(r.values[0] ?? "")).filter(Boolean);
      const parsed = parseGrants(lines);
      setOriginal(parsed.scopes);
      setDraft(parsed.scopes.map((s) => ({ ...s, privs: new Set(s.privs) })));
      setUnparsed(parsed.unparsed);
      setDatabases(dbs);
    } catch (err) {
      setLoadError(String(err));
    } finally {
      setLoading(false);
    }
    // account 由 name/host 派生，二者不变时无需重新加载
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionId, name, host]);

  useEffect(() => {
    load();
  }, [load]);

  const dirty = useMemo(
    () => buildStatements(account, original, draft).length > 0,
    [account, original, draft]
  );

  const updateScope = (scope: string, patch: Partial<ScopeGrants>) =>
    setDraft((prev) => prev.map((s) => (s.scope === scope ? { ...s, ...patch } : s)));

  const togglePriv = (scope: string, priv: string) =>
    setDraft((prev) =>
      prev.map((s) => {
        if (s.scope !== scope) return s;
        const privs = new Set(s.privs);
        if (privs.has(priv)) privs.delete(priv);
        else privs.add(priv);
        return { ...s, privs };
      })
    );

  const removeScope = (scope: string) =>
    setDraft((prev) => prev.filter((s) => s.scope !== scope));

  const addDbScope = () => {
    const db = addDbValue.trim();
    if (!db || draft.some((s) => s.scope === `${db}.*`)) return;
    setDraft((prev) => [...prev, { scope: `${db}.*`, dbName: db, privs: new Set(["SELECT"]), grantOption: false }]);
    setAddDbValue("");
  };

  const save = async () => {
    const stmts = buildStatements(account, original, draft);
    if (stmts.length === 0) return;
    setSaving(true);
    try {
      await tauriService.executeBatch(connectionId, undefined, stmts);
      window.alert(t("priv.saved"));
      onSaved();
    } catch (err) {
      window.alert(t("priv.saveFailed", { error: String(err) }));
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return (
      <div className="px-8 py-3 bg-muted/20 border-t border-border/50">
        <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
      </div>
    );
  }
  if (loadError) {
    return (
      <div className="px-8 py-3 bg-muted/20 border-t border-border/50 text-xs text-red-600">
        {t("common.loadFailed", { error: loadError })}
      </div>
    );
  }

  const candidateDbs = databases.filter((db) => !draft.some((s) => s.scope === `${db}.*`));

  return (
    <div className="px-4 py-3 bg-muted/20 border-t border-border/50 space-y-3">
      <div className="text-xs text-muted-foreground">
        {t("priv.account")} <span className="font-mono">{name}@{host ?? "%"}</span>
      </div>

      {draft.map((s) => (
        <div key={s.scope} className="rounded-md border bg-background">
          <div className="flex items-center gap-2 px-3 py-1.5 border-b bg-muted/30">
            <span className="text-xs font-medium">
              {s.scope === "*.*" ? t("priv.global") : t("priv.dbScope", { db: s.dbName ?? "" })}
            </span>
            {s.scope !== "*.*" && (
              <button
                className="ml-auto text-gray-400 hover:text-red-500"
                title={t("priv.removeScope")}
                onClick={() => removeScope(s.scope)}
              >
                <Trash2 className="h-3 w-3" />
              </button>
            )}
          </div>
          <div className="px-3 py-2 space-y-1.5">
            {PRIV_GROUPS.map((g) => (
              <div key={g.key} className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="text-[10px] text-muted-foreground w-8 flex-shrink-0">{t(g.key)}</span>
                {g.privs.map((p) => (
                  <label
                    key={p}
                    className={cn(
                      "flex items-center gap-1 px-1.5 py-0.5 rounded border text-[11px] cursor-pointer select-none",
                      s.privs.has(p)
                        ? "bg-blue-500/10 border-blue-400 text-blue-600"
                        : "text-muted-foreground"
                    )}
                  >
                    <input
                      type="checkbox"
                      className="hidden"
                      checked={s.privs.has(p)}
                      onChange={() => togglePriv(s.scope, p)}
                    />
                    {p}
                  </label>
                ))}
              </div>
            ))}
            <label className="flex items-center gap-1.5 text-[11px] text-muted-foreground cursor-pointer pt-0.5">
              <input
                type="checkbox"
                checked={s.grantOption}
                onChange={(e) => updateScope(s.scope, { grantOption: e.target.checked })}
              />
              {t("priv.grantOption")}
            </label>
          </div>
        </div>
      ))}

      {/* 添加库权限 */}
      {candidateDbs.length > 0 && (
        <div className="flex items-center gap-2">
          <select
            className="h-7 px-1.5 rounded border bg-background text-xs"
            value={addDbValue}
            onChange={(e) => setAddDbValue(e.target.value)}
          >
            <option value="">{t("priv.selectDb")}</option>
            {candidateDbs.map((db) => (
              <option key={db} value={db}>
                {db}
              </option>
            ))}
          </select>
          <Button variant="outline" size="sm" className="h-7 text-xs" disabled={!addDbValue} onClick={addDbScope}>
            <Plus className="h-3 w-3 mr-1" />
            {t("priv.addDb")}
          </Button>
        </div>
      )}

      {/* 未解析的授权行：原样保留 */}
      {unparsed.length > 0 && (
        <div className="text-[11px] text-muted-foreground">
          <div>{t("priv.unparsed")}</div>
          {unparsed.map((line, i) => (
            <div key={i} className="font-mono break-all opacity-70">
              {line}
            </div>
          ))}
        </div>
      )}

      <div className="flex items-center gap-2 pt-1">
        <Button size="sm" className="h-7 text-xs" onClick={save} disabled={!dirty || saving}>
          {saving ? <Loader2 className="h-3 w-3 animate-spin mr-1" /> : <Check className="h-3 w-3 mr-1" />}
          {t("priv.save")}
        </Button>
        <Button variant="outline" size="sm" className="h-7 text-xs" onClick={onSaved}>
          {t("common.cancel")}
        </Button>
      </div>
    </div>
  );
}
