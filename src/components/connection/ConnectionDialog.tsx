import { useEffect, useState } from "react";
import { ConnectionConfig } from "@/types/database";
import { tauriService } from "@/services/tauriService";
import { useConnectionStore } from "@/stores/connectionStore";
import { Button, Input, Label, Select, Dialog, Alert } from "@/components/ui";
import { Database, TestTube } from "lucide-react";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";

interface ConnectionDialogProps {
  open: boolean;
  onClose: () => void;
  /** 传入则为编辑模式（密码留空表示保持不变） */
  editing?: ConnectionConfig | null;
}

// 后端已支持 MySQL / PostgreSQL / SQLite / SQL Server
const DRIVER_OPTIONS = [
  { value: "mysql", label: "MySQL" },
  { value: "postgres", label: "PostgreSQL" },
  { value: "sqlite", label: "SQLite" },
  { value: "sqlserver", label: "SQL Server" },
];

const DEFAULT_PORTS: Record<string, number> = {
  mysql: 3306,
  postgres: 5432,
  sqlite: 0,
  sqlserver: 1433,
};

const DEFAULT_CONFIG: ConnectionConfig = {
  name: "",
  driverType: "mysql",
  host: "localhost",
  port: 3306,
  user: "root",
  password: "",
  database: "",
  savePassword: true,
  groupName: "",
  color: "gray",
};

const COLOR_OPTIONS = [
  { value: "gray", label: "灰", className: "bg-gray-400" },
  { value: "red", label: "红", className: "bg-red-500" },
  { value: "orange", label: "橙", className: "bg-orange-500" },
  { value: "green", label: "绿", className: "bg-green-500" },
  { value: "blue", label: "蓝", className: "bg-blue-500" },
  { value: "purple", label: "紫", className: "bg-purple-500" },
];

export function ConnectionDialog({ open, onClose, editing }: ConnectionDialogProps) {
  const [config, setConfig] = useState<ConnectionConfig>(DEFAULT_CONFIG);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ success: boolean; message: string } | null>(null);
  const [connecting, setConnecting] = useState(false);
  const upsertSaved = useConnectionStore((s) => s.upsertSaved);
  const markLive = useConnectionStore((s) => s.markLive);
  const isEditMode = !!editing;

  // 打开对话框时初始化：编辑模式回填（密码置空 = 保持不变），新建模式重置
  useEffect(() => {
    if (open) {
      setConfig(editing ? { ...editing, password: "" } : DEFAULT_CONFIG);
      setTestResult(null);
    }
  }, [open, editing]);

  const isSqlite = config.driverType === "sqlite";

  // SQLite 只需要名称 + 文件路径；其余类型名称、主机、端口必填
  const isValid = isSqlite
    ? config.name.trim() !== "" && (config.database ?? "").trim() !== ""
    : config.name.trim() !== "" &&
      config.host.trim() !== "" &&
      config.port > 0 &&
      config.port <= 65535;

  const updateField = <K extends keyof ConnectionConfig>(field: K, value: ConnectionConfig[K]) => {
    setConfig((prev) => ({ ...prev, [field]: value }));
    setTestResult(null);
  };

  const handleTest = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      await tauriService.testConnection(config);
      setTestResult({ success: true, message: "连接测试成功！" });
    } catch (error) {
      setTestResult({ success: false, message: `连接失败: ${error}` });
    }
    setTesting(false);
  };

  const handleConnect = async () => {
    setConnecting(true);
    try {
      const normalized = {
        ...config,
        groupName: undefined,
      };
      if (isEditMode) {
        // 编辑：更新持久化配置（密码留空则后端保留原密码），断开后由用户重新点击连接
        await tauriService.updateConnection(normalized);
        upsertSaved({ ...normalized, password: "" });
        markLive(normalized.id!, false);
      } else {
        // 新建：持久化保存（密码加密落盘）并建立活动连接
        const id = await tauriService.saveConnection(normalized);
        upsertSaved({ ...normalized, id, password: "" });
        markLive(id, true);
      }
      setConfig(DEFAULT_CONFIG);
      setTestResult(null);
      onClose();
    } catch (error) {
      setTestResult({ success: false, message: `${isEditMode ? "保存" : "连接"}失败: ${error}` });
    }
    setConnecting(false);
  };

  return (
    <Dialog
      open={open}
      onClose={onClose}
      title={isEditMode ? "编辑数据库连接" : "新建数据库连接"}
      footer={
        <>
          <Button variant="outline" onClick={onClose}>
            取消
          </Button>
          <Button variant="outline" onClick={handleTest} loading={testing} disabled={!isValid}>
            <TestTube className="mr-2 h-4 w-4" />
            测试连接
          </Button>
          <Button onClick={handleConnect} loading={connecting} disabled={!isValid}>
            <Database className="mr-2 h-4 w-4" />
            {isEditMode ? "保存修改" : "保存并连接"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {testResult && (
          <Alert variant={testResult.success ? "default" : "destructive"}>
            {testResult.message}
          </Alert>
        )}

        <div className="space-y-2">
          <Label>连接名称</Label>
          <Input
            placeholder="例如: 本地 MySQL"
            value={config.name}
            onChange={(e) => updateField("name", e.target.value)}
          />
        </div>

        <div className="space-y-2">
          <Label>数据库类型</Label>
          <Select
            options={DRIVER_OPTIONS}
            value={config.driverType}
            onChange={(e) => {
              const driver = e.target.value as ConnectionConfig["driverType"];
              updateField("driverType", driver);
              // 端口跟着驱动类型联动（当前值仍是旧默认端口时才覆盖，保留用户手改的值）
              if (Object.values(DEFAULT_PORTS).includes(config.port)) {
                updateField("port", DEFAULT_PORTS[driver] ?? config.port);
              }
            }}
          />
        </div>

        {isSqlite ? (
          <div className="space-y-2">
            <Label>数据库文件</Label>
            <div className="flex gap-2">
              <Input
                className="flex-1"
                placeholder="选择或输入 .db / .sqlite 文件路径（不存在则创建）"
                value={config.database ?? ""}
                onChange={(e) => updateField("database", e.target.value)}
              />
              <Button
                variant="outline"
                type="button"
                onClick={async () => {
                  const selected = await openFileDialog({
                    multiple: false,
                    filters: [
                      { name: "SQLite 数据库", extensions: ["db", "sqlite", "sqlite3"] },
                      { name: "所有文件", extensions: ["*"] },
                    ],
                  });
                  if (typeof selected === "string") updateField("database", selected);
                }}
              >
                浏览…
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              SQLite 是本地文件数据库，无需主机/账号；路径不存在时会创建新库。
            </p>
          </div>
        ) : (
          <>
            <div className="grid grid-cols-3 gap-4">
              <div className="col-span-2 space-y-2">
                <Label>主机</Label>
                <Input
                  placeholder="localhost"
                  value={config.host}
                  onChange={(e) => updateField("host", e.target.value)}
                />
              </div>
              <div className="space-y-2">
                <Label>端口</Label>
                <Input
                  type="number"
                  value={config.port}
                  onChange={(e) => updateField("port", parseInt(e.target.value) || 0)}
                />
              </div>
            </div>

            <div className="space-y-2">
              <Label>用户名</Label>
              <Input
                placeholder={config.driverType === "sqlserver" ? "sa" : "root"}
                value={config.user}
                onChange={(e) => updateField("user", e.target.value)}
              />
            </div>

            <div className="space-y-2">
              <Label>密码</Label>
              <Input
                type="password"
                placeholder={isEditMode ? "留空表示保持不变" : ""}
                value={config.password}
                onChange={(e) => updateField("password", e.target.value)}
              />
            </div>

            <div className="space-y-2">
              <Label>数据库</Label>
              <Input
                placeholder="可选，留空显示所有数据库"
                value={config.database}
                onChange={(e) => updateField("database", e.target.value)}
              />
            </div>
          </>
        )}

        <div className="space-y-2">
          <Label>颜色标记</Label>
          <div className="flex items-center gap-1.5 h-9">
            {COLOR_OPTIONS.map((c) => (
              <button
                key={c.value}
                type="button"
                title={c.label}
                onClick={() => updateField("color", c.value)}
                className={`w-6 h-6 rounded-full ${c.className} ${
                  config.color === c.value
                    ? "ring-2 ring-offset-2 ring-blue-500"
                    : "opacity-50 hover:opacity-100"
                }`}
              />
            ))}
          </div>
        </div>

        <div className="flex items-center gap-2">
          <input
            type="checkbox"
            id="savePassword"
            checked={config.savePassword}
            onChange={(e) => updateField("savePassword", e.target.checked)}
          />
          <Label htmlFor="savePassword" className="cursor-pointer">记住密码</Label>
        </div>
      </div>
    </Dialog>
  );
}
