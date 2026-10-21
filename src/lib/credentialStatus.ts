import type { CredentialHealth } from "@/types/credential";

export interface CredentialNotice {
  title: string;
  recovery: string;
  failed: boolean;
}

/** 恢复提示只说明现有状态，不把读取失败解释为空密码或触发初始化。 */
export function credentialNotice(health: CredentialHealth | null): CredentialNotice | null {
  if (!health) return null;
  if (health.state === "ready") {
    return health.backend === "file" ? {
      title: "凭据使用本地密钥文件",
      recovery: "备份连接配置时请同时保留原密钥文件。使用支持系统钥匙串的平台打开时会迁移原密钥。",
      failed: false,
    } : null;
  }
  const recovery: Record<string, [string, string]> = {
    locked: ["系统钥匙串已锁定", "解锁系统钥匙串后重启应用。已保存连接的名称等信息仍可查看和修改。"],
    permission: ["凭据访问受限", "解锁系统钥匙串或恢复访问权限后重启应用。已保存的密码会保留。"],
    unavailable: ["凭据服务暂不可用", "恢复系统凭据服务后重启应用。已保存的密码会保留。"],
    corrupt: ["凭据或配置校验失败", "请从配套备份恢复原密钥和连接配置后重启应用。已有文件会保留。"],
    missing: ["原凭据密钥缺失", "请恢复原钥匙串条目或配套密钥备份后重启应用。已有连接配置会保留。"],
    conflict: ["凭据密钥不一致", "请恢复属于同一套配置的原密钥后重启应用。现有密钥材料会保留。"],
    unsupported: ["当前平台无法读取凭据", "请在支持原凭据后端的平台恢复配套配置，再重新打开应用。"],
    io: ["凭据持久化未完成", "检查配置目录权限和可用空间后重启应用；尚未确认迁移的旧密钥会保留。"],
    migration_pending: ["凭据迁移待完成", "恢复系统钥匙串与配置目录的访问权限后重启应用，继续验证原密钥。"],
  };
  const [title, hint] = recovery[health.state] ?? ["凭据状态无法确认", "恢复系统钥匙串或配置目录访问后重启应用。已有凭据会保留。"];
  return { title, recovery: hint, failed: true };
}
