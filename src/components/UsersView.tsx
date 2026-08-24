import { useCallback, useEffect, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { useConnectionStore } from "@/stores/connectionStore";
import { DbUserInfo, CurrentUserInfo } from "@/types/database";
import { Button } from "@/components/ui";
import { cn } from "@/lib/utils";
import { PrivilegeEditor } from "@/components/PrivilegeEditor";
import {
  ChevronDown,
  ChevronRight,
  KeyRound,
  Loader2,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
  UserRound,
} from "lucide-react";

interface UsersViewProps {
  connectionId: string;
}

/** 权限预设 */
const PRESETS: { value: string; label: string; privs: string[] }[] = [
  { value: "readonly", label: "只读（SELECT）", privs: ["SELECT"] },
  { value: "readwrite", label: "读写（SELECT/INSERT/UPDATE/DELETE）", privs: ["SELECT", "INSERT", "UPDATE", "DELETE"] },
  { value: "all", label: "全部权限（ALL PRIVILEGES）", privs: ["ALL PRIVILEGES"] },
  { value: "custom", label: "自定义", privs: [] },
];

const CUSTOM_PRIVS = ["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP", "ALTER", "INDEX", "CREATE VIEW", "TRIGGER"];

/** 单引号转义，防注入 */
const sqlStr = (s: string) => s.replace(/'/g, "''");

/** 用户与权限管理页：当前用户卡片 + 账号列表 + MySQL 账号操作 */
export function UsersView({ connectionId }: UsersViewProps) {
  const driverType =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.driverType ?? "mysql";
  const connName =
    useConnectionStore.getState().saved.find((c) => c.id === connectionId)?.name ?? connectionId;
  // MySQL/MariaDB 支持完整账号管理，其余驱动 v1 只读
  const manageable = driverType === "mysql" || driverType === "mariadb";

  const [currentUser, setCurrentUser] = useState<CurrentUserInfo | null>(null);
  const [users, setUsers] = useState<DbUserInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 展开权限明细的账号 key（name@host） */
  const [grantsFor, setGrantsFor] = useState<string | null>(null);
  const [grantsText, setGrantsText] = useState<string[]>([]);
  const [grantsLoading, setGrantsLoading] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [cu, us] = await Promise.all([
        tauriService.getCurrentUser(connectionId),
        tauriService.getUsers(connectionId),
      ]);
      setCurrentUser(cu);
      setUsers(us);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [connectionId]);

  useEffect(() => {
    load();
  }, [load]);

  const userKey = (u: DbUserInfo) => (u.host ? `${u.name}@${u.host}` : u.name);

  /** 查看/编辑某账号权限：MySQL 用可视化编辑器（自加载），其余驱动只看 SHOW GRANTS 文本 */
  const toggleGrants = async (u: DbUserInfo) => {
    const key = userKey(u);
    if (grantsFor === key) {
      setGrantsFor(null);
      return;
    }
    setGrantsFor(key);
    if (manageable) return; // 可视化编辑器自行加载
    setGrantsLoading(true);
    try {
      const sql = `SHOW GRANTS FOR '${sqlStr(u.name)}'@'${sqlStr(u.host ?? "%")}'`;
      const result = await tauriService.executeQuery(connectionId, sql);
      setGrantsText(
        result.rows.map((r) => String(r.values[0] ?? "")).filter(Boolean)
      );
    } catch (err) {
      setGrantsText([`加载失败: ${err}`]);
    } finally {
      setGrantsLoading(false);
    }
  };

  const changePassword = async (u: DbUserInfo) => {
    const pwd = window.prompt(`为 ${userKey(u)} 设置新密码:`);
    if (pwd === null) return;
    if (!pwd) {
      window.alert("密码不能为空");
      return;
    }
    try {
      await tauriService.executeQuery(
        connectionId,
        `ALTER USER '${sqlStr(u.name)}'@'${sqlStr(u.host ?? "%")}' IDENTIFIED BY '${sqlStr(pwd)}';`
      );
      window.alert("密码已更新");
    } catch (err) {
      window.alert(`修改密码失败: ${err}`);
    }
  };

  const dropUser = async (u: DbUserInfo) => {
    if (!window.confirm(`确定删除账号 ${userKey(u)} 吗？此操作不可恢复！`)) return;
    try {
      await tauriService.executeQuery(
        connectionId,
        `DROP USER '${sqlStr(u.name)}'@'${sqlStr(u.host ?? "%")}';`
      );
      load();
    } catch (err) {
      window.alert(`删除失败: ${err}`);
    }
  };

  return (
    <div className="flex flex-col h-full min-h-0">
      {/* 头部 */}
      <div className="flex items-center gap-2 px-3 py-2 border-b bg-muted/20 flex-shrink-0">
        <ShieldCheck className="h-4 w-4 text-blue-500" />
        <span className="text-sm font-medium">用户与权限</span>
        <span className="text-xs text-muted-foreground">{connName}</span>
        <div className="ml-auto flex items-center gap-1">
          {manageable && (
            <Button
              size="sm"
              variant={createOpen ? "default" : "outline"}
              className="h-7"
              onClick={() => setCreateOpen((v) => !v)}
            >
              <Plus className="mr-1 h-3.5 w-3.5" />
              新建用户
            </Button>
          )}
          <Button variant="ghost" size="sm" className="h-7" onClick={load} disabled={loading}>
            {loading ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
            <span className="ml-1">刷新</span>
          </Button>
        </div>
      </div>

      {error && (
        <div className="px-3 py-1.5 bg-red-50 text-red-700 text-xs border-b border-red-200 flex-shrink-0">
          加载失败: {error}
        </div>
      )}

      <div className="flex-1 overflow-auto custom-scrollbar">
        {/* 当前用户卡片 */}
        <div className="m-3 rounded-md border bg-muted/10">
          <div className="flex items-center gap-2 px-3 py-2 border-b">
            <UserRound className="h-4 w-4 text-green-600" />
            <span className="text-sm font-medium">当前用户</span>
            <span className="font-mono text-xs text-muted-foreground">{currentUser?.user ?? "…"}</span>
          </div>
          <div className="px-3 py-2">
            {currentUser?.grants.length ? (
              <ul className="space-y-1">
                {currentUser.grants.map((g, i) => (
                  <li key={i} className="font-mono text-xs leading-5 break-all text-muted-foreground">
                    {g}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-xs text-muted-foreground">{loading ? "加载中…" : "无授权信息"}</p>
            )}
          </div>
        </div>

        {/* 新建用户面板（仅 MySQL） */}
        {createOpen && manageable && (
          <CreateUserPanel
            connectionId={connectionId}
            onCreated={() => {
              setCreateOpen(false);
              load();
            }}
            onCancel={() => setCreateOpen(false)}
          />
        )}

        {/* 账号列表 */}
        <div className="mx-3 mb-3 rounded-md border overflow-hidden">
          <div className="px-3 py-1.5 border-b bg-muted/30 text-xs text-muted-foreground">
            账号列表（{users.length}）
            {!manageable && driverType !== "sqlite" && (
              <span className="ml-2">· 该数据库类型暂只读，账号管理后续版本支持</span>
            )}
            {driverType === "sqlite" && (
              <span className="ml-2">· SQLite 无账号体系</span>
            )}
          </div>
          {users.length === 0 ? (
            <div className="px-3 py-6 text-center text-xs text-muted-foreground">
              {loading ? "加载中…" : "无账号"}
            </div>
          ) : (
            users.map((u) => {
              const key = userKey(u);
              const expanded = grantsFor === key;
              return (
                <div key={key} className="border-b border-border/50 last:border-b-0">
                  <div className="flex items-center gap-2 px-3 py-1.5 text-sm hover:bg-muted/30">
                    <UserRound className="h-3.5 w-3.5 text-muted-foreground flex-shrink-0" />
                    <span className="font-mono text-xs">{key}</span>
                    {u.note && (
                      <span className="text-[10px] px-1 rounded bg-muted text-muted-foreground">{u.note}</span>
                    )}
                    <div className="ml-auto flex items-center gap-0.5">
                      <Button variant="ghost" size="sm" className="h-6 px-2 text-xs" onClick={() => toggleGrants(u)}>
                        {expanded ? <ChevronDown className="h-3 w-3 mr-0.5" /> : <ChevronRight className="h-3 w-3 mr-0.5" />}
                        权限
                      </Button>
                      {manageable && (
                        <>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-6 px-2 text-xs"
                            title="修改密码"
                            onClick={() => changePassword(u)}
                          >
                            <KeyRound className="h-3 w-3 mr-0.5" />
                            改密码
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            className="h-6 px-2 text-xs text-red-500 hover:text-red-600"
                            title="删除账号"
                            onClick={() => dropUser(u)}
                          >
                            <Trash2 className="h-3 w-3" />
                          </Button>
                        </>
                      )}
                    </div>
                  </div>
                  {expanded && manageable && (
                    <PrivilegeEditor
                      connectionId={connectionId}
                      name={u.name}
                      host={u.host}
                      onSaved={() => {
                        setGrantsFor(null);
                        load();
                      }}
                    />
                  )}
                  {expanded && !manageable && (
                    <div className="px-8 py-2 bg-muted/20 border-t border-border/50">
                      {grantsLoading ? (
                        <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />
                      ) : (
                        <ul className="space-y-1">
                          {grantsText.map((g, i) => (
                            <li key={i} className="font-mono text-xs leading-5 break-all text-muted-foreground">
                              {g}
                            </li>
                          ))}
                        </ul>
                      )}
                    </div>
                  )}
                </div>
              );
            })
          )}
        </div>
      </div>
    </div>
  );
}

/** 新建用户面板（MySQL）：账号 + 主机 + 密码 + 作用域 + 权限预设/自定义 */
function CreateUserPanel({
  connectionId,
  onCreated,
  onCancel,
}: {
  connectionId: string;
  onCreated: () => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [host, setHost] = useState("%");
  const [password, setPassword] = useState("");
  const [scope, setScope] = useState("*.*");
  const [preset, setPreset] = useState("readonly");
  const [customPrivs, setCustomPrivs] = useState<string[]>(["SELECT"]);
  const [databases, setDatabases] = useState<string[]>([]);
  const [submitting, setSubmitting] = useState(false);

  useEffect(() => {
    tauriService.getDatabases(connectionId).then(setDatabases).catch(() => {});
  }, [connectionId]);

  const togglePriv = (p: string) =>
    setCustomPrivs((prev) => (prev.includes(p) ? prev.filter((x) => x !== p) : [...prev, p]));

  const selectedPrivs =
    preset === "custom" ? customPrivs : PRESETS.find((p) => p.value === preset)?.privs ?? [];

  const submit = async () => {
    if (!name.trim()) {
      window.alert("用户名不能为空");
      return;
    }
    if (!password) {
      window.alert("密码不能为空");
      return;
    }
    if (selectedPrivs.length === 0) {
      window.alert("请至少选择一项权限");
      return;
    }
    setSubmitting(true);
    const u = `'${sqlStr(name.trim())}'@'${sqlStr(host.trim() || "%")}'`;
    try {
      await tauriService.executeQuery(
        connectionId,
        `CREATE USER IF NOT EXISTS ${u} IDENTIFIED BY '${sqlStr(password)}';`
      );
      // CREATE 成功后授权失败要提示清楚（用户已建好）
      try {
        await tauriService.executeQuery(
          connectionId,
          `GRANT ${selectedPrivs.join(", ")} ON ${scope === "*.*" ? "*.*" : `\`${scope.replace(/`/g, "``")}\`.*`} TO ${u};`
        );
      } catch (err) {
        window.alert(`用户已创建，但授权失败: ${err}\n可稍后在权限列表手动调整`);
        onCreated();
        return;
      }
      window.alert(`用户 ${name} 创建成功`);
      onCreated();
    } catch (err) {
      window.alert(`创建用户失败: ${err}`);
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="mx-3 mb-3 rounded-md border bg-muted/10 p-3 space-y-2">
      <div className="text-sm font-medium">新建用户</div>
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <input
          className="h-7 w-36 px-2 rounded border bg-background focus:outline-none"
          placeholder="用户名"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <span className="text-muted-foreground">@</span>
        <input
          className="h-7 w-28 px-2 rounded border bg-background focus:outline-none"
          placeholder="主机"
          title="% 表示任意主机；localhost 仅本机"
          value={host}
          onChange={(e) => setHost(e.target.value)}
        />
        <input
          className="h-7 w-40 px-2 rounded border bg-background focus:outline-none"
          type="password"
          placeholder="密码"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
        <select
          className="h-7 px-1 rounded border bg-background focus:outline-none"
          title="权限作用域"
          value={scope}
          onChange={(e) => setScope(e.target.value)}
        >
          <option value="*.*">整个实例（*.*）</option>
          {databases.map((db) => (
            <option key={db} value={db}>
              库：{db}
            </option>
          ))}
        </select>
        <select
          className="h-7 px-1 rounded border bg-background focus:outline-none"
          value={preset}
          onChange={(e) => setPreset(e.target.value)}
        >
          {PRESETS.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label}
            </option>
          ))}
        </select>
      </div>
      {preset === "custom" && (
        <div className="flex flex-wrap gap-2">
          {CUSTOM_PRIVS.map((p) => (
            <label
              key={p}
              className={cn(
                "flex items-center gap-1 px-2 py-0.5 rounded border text-xs cursor-pointer",
                customPrivs.includes(p) ? "bg-blue-500/10 border-blue-400 text-blue-600" : "text-muted-foreground"
              )}
            >
              <input
                type="checkbox"
                className="hidden"
                checked={customPrivs.includes(p)}
                onChange={() => togglePriv(p)}
              />
              {p}
            </label>
          ))}
        </div>
      )}
      <div className="flex items-center gap-2">
        <Button size="sm" className="h-7" onClick={submit} loading={submitting}>
          创建
        </Button>
        <Button size="sm" variant="outline" className="h-7" onClick={onCancel}>
          取消
        </Button>
      </div>
    </div>
  );
}
