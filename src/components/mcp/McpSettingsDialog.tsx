import { useEffect, useState } from "react";
import { tauriService } from "@/services/tauriService";
import { useConnectionStore } from "@/stores/connectionStore";
import { McpSettingsSnapshot } from "@/types/database";
import { Button, Input, Label, Dialog, Alert } from "@/components/ui";
import { useT } from "@/i18n";

interface McpSettingsDialogProps {
  open: boolean;
  onClose: () => void;
}

/** 工具名与界面文案的对应（工具原名以等宽字体展示，便于与 MCP 客户端配置对照） */
const MCP_TOOL_ITEMS = [
  { name: "list_connections", labelKey: "mcpSettings.toolListConnections" },
  { name: "list_databases", labelKey: "mcpSettings.toolListDatabases" },
  { name: "list_tables", labelKey: "mcpSettings.toolListTables" },
  { name: "describe_table", labelKey: "mcpSettings.toolDescribeTable" },
  { name: "query_scalar", labelKey: "mcpSettings.toolQueryScalar" },
  { name: "read_table_rows", labelKey: "mcpSettings.toolReadTableRows" },
  { name: "execute_readonly_sql", labelKey: "mcpSettings.toolExecuteReadonlySql" },
] as const;

/**
 * MCP 设置对话框：启用开关、端口、工具开关、连接访问白名单。
 * 保存时统一提交：设置热应用（停用→停止服务，端口变更→重启），连接开关写入连接配置。
 */
export function McpSettingsDialog({ open, onClose }: McpSettingsDialogProps) {
  const t = useT();
  const saved = useConnectionStore((s) => s.saved);
  const upsertSaved = useConnectionStore((s) => s.upsertSaved);
  const [enabled, setEnabled] = useState(true);
  const [port, setPort] = useState("48123");
  const [disabledTools, setDisabledTools] = useState<Set<string>>(new Set());
  /** 拒绝 MCP 访问的连接 id 集合（勾选 = 允许） */
  const [deniedConnections, setDeniedConnections] = useState<Set<string>>(new Set());
  const [status, setStatus] = useState<McpSettingsSnapshot | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 打开时加载当前设置；连接开关初始状态取自已保存连接的 mcpAllowed（缺省视为允许）
  useEffect(() => {
    if (!open) return;
    setError(null);
    setStatus(null);
    setDeniedConnections(
      new Set(
        useConnectionStore
          .getState()
          .saved.filter((c) => c.mcpAllowed === false)
          .map((c) => c.id!)
      )
    );
    tauriService
      .getMcpSettings()
      .then((snap) => {
        setStatus(snap);
        setEnabled(snap.enabled);
        setPort(String(snap.port));
        setDisabledTools(new Set(snap.disabledTools));
      })
      .catch((e) => setError(String(e)));
  }, [open]);

  const toggleTool = (name: string, checked: boolean) => {
    setDisabledTools((prev) => {
      const next = new Set(prev);
      if (checked) next.delete(name);
      else next.add(name);
      return next;
    });
  };

  const toggleConnection = (id: string, allowed: boolean) => {
    setDeniedConnections((prev) => {
      const next = new Set(prev);
      if (allowed) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const portNumber = Number(port);
  const isValid = !enabled || (Number.isInteger(portNumber) && portNumber >= 1 && portNumber <= 65535);

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      const snapshot = await tauriService.saveMcpSettings({
        enabled,
        port: portNumber,
        disabledTools: [...disabledTools],
      });
      setStatus(snapshot);
      // 连接开关：只提交发生变化的项
      for (const conn of useConnectionStore.getState().saved) {
        const id = conn.id!;
        const allowed = !deniedConnections.has(id);
        if ((conn.mcpAllowed === false) === allowed) continue;
        await tauriService.setConnectionMcpAllowed(id, allowed);
        upsertSaved({ ...conn, mcpAllowed: allowed });
      }
      onClose();
    } catch (e) {
      setError(t("mcpSettings.saveFailed", { error: String(e) }));
    }
    setSaving(false);
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={t("mcpSettings.title")}
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button onClick={handleSave} loading={saving} disabled={!isValid}>
            {t("mcpSettings.save")}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}

        {/* 运行状态 */}
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          {status?.running ? (
            <>
              <span className="w-2 h-2 rounded-full bg-green-500 flex-shrink-0" />
              <span className="break-all">{t("mcpSettings.runningAt", { url: status.url ?? "" })}</span>
            </>
          ) : (
            <>
              <span className="w-2 h-2 rounded-full bg-gray-400 flex-shrink-0" />
              <span>{t("mcpSettings.stopped")}</span>
            </>
          )}
        </div>

        {/* 总开关 + 端口 */}
        <label className="flex items-center gap-2 text-sm cursor-pointer select-none">
          <input
            type="checkbox"
            className="h-4 w-4 accent-blue-600"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
          />
          {t("mcpSettings.enableService")}
        </label>

        <div className="space-y-2">
          <Label htmlFor="mcp-port">{t("mcpSettings.port")}</Label>
          <Input
            id="mcp-port"
            type="number"
            min={1}
            max={65535}
            value={port}
            disabled={!enabled}
            onChange={(e) => setPort(e.target.value)}
          />
          <p className="text-xs text-muted-foreground">{t("mcpSettings.portHint")}</p>
        </div>

        {/* 工具开关 */}
        <div className="space-y-2">
          <Label>{t("mcpSettings.tools")}</Label>
          <div className="space-y-1.5">
            {MCP_TOOL_ITEMS.map((item) => (
              <label key={item.name} className="flex items-center gap-2 text-sm cursor-pointer select-none">
                <input
                  type="checkbox"
                  className="h-4 w-4 accent-blue-600"
                  checked={!disabledTools.has(item.name)}
                  disabled={!enabled}
                  onChange={(e) => toggleTool(item.name, e.target.checked)}
                />
                <span>{t(item.labelKey)}</span>
                <span className="text-xs text-muted-foreground font-mono">{item.name}</span>
              </label>
            ))}
          </div>
        </div>

        {/* 连接白名单 */}
        <div className="space-y-2">
          <Label>{t("mcpSettings.connections")}</Label>
          {saved.length === 0 ? (
            <p className="text-xs text-muted-foreground">{t("mcpSettings.noConnections")}</p>
          ) : (
            <div className="space-y-1.5 max-h-40 overflow-auto custom-scrollbar pr-1">
              {saved.map((conn) => (
                <label
                  key={conn.id}
                  className="flex items-center gap-2 text-sm cursor-pointer select-none min-w-0"
                >
                  <input
                    type="checkbox"
                    className="h-4 w-4 accent-blue-600 flex-shrink-0"
                    checked={!deniedConnections.has(conn.id!)}
                    onChange={(e) => toggleConnection(conn.id!, e.target.checked)}
                  />
                  <span className="truncate">{conn.name}</span>
                  <span className="text-xs text-muted-foreground truncate">
                    {conn.driverType}
                    {conn.database ? ` · ${conn.database}` : ""}
                  </span>
                </label>
              ))}
            </div>
          )}
          <p className="text-xs text-muted-foreground">{t("mcpSettings.connectionsHint")}</p>
        </div>
      </div>
    </Dialog>
  );
}
