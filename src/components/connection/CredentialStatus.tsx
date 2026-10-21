import { useEffect } from "react";
import { useConnectionStore } from "@/stores/connectionStore";
import { credentialNotice } from "@/lib/credentialStatus";

/** 连接树和编辑框共用同一状态，故障时仍保留元数据编辑能力。 */
export function CredentialStatus({ className = "", compact = false }: { className?: string; compact?: boolean }) {
  const health = useConnectionStore((state) => state.credentialHealth);
  const refresh = useConnectionStore((state) => state.refreshCredentialHealth);
  useEffect(() => { void refresh(); }, [refresh]);
  const notice = credentialNotice(health);
  if (!notice) return null;
  return (
    <div
      role={notice.failed ? "alert" : "status"}
      className={`space-y-1 border border-amber-300 bg-amber-50 p-3 text-xs text-amber-950 dark:border-amber-800 dark:bg-amber-950/30 dark:text-amber-100 ${className}`}
    >
      <p className="font-medium">{notice.title}</p>
      <p>{notice.recovery}</p>
      {!compact && health?.message && (
        <details>
          <summary className="cursor-pointer">查看详情</summary>
          <p className="break-words pt-1">{health.message}</p>
        </details>
      )}
    </div>
  );
}
