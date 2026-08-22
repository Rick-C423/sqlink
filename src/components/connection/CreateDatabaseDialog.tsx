import { useState } from "react";
import { Dialog, Button, Input, Label, Select, Alert } from "@/components/ui";
import { quoteIdent } from "@/lib/utils";
import { tauriService } from "@/services/tauriService";

/** MySQL 常用字符集与对应排序规则（首项为默认） */
const MYSQL_CHARSETS = ["utf8mb4", "utf8", "gbk", "latin1", "ascii"];
const MYSQL_COLLATIONS: Record<string, string[]> = {
  utf8mb4: ["utf8mb4_0900_ai_ci", "utf8mb4_general_ci", "utf8mb4_unicode_ci", "utf8mb4_bin"],
  utf8: ["utf8_general_ci", "utf8_unicode_ci", "utf8_bin"],
  gbk: ["gbk_chinese_ci", "gbk_bin"],
  latin1: ["latin1_swedish_ci", "latin1_bin"],
  ascii: ["ascii_general_ci", "ascii_bin"],
};

const PG_ENCODINGS = ["UTF8", "SQL_ASCII", "LATIN1", "WIN1252"];

interface CreateDatabaseDialogProps {
  open: boolean;
  connectionId: string;
  driverType: string;
  onClose: () => void;
  /** 创建成功后回调（用于刷新连接节点） */
  onCreated: () => void;
}

export function CreateDatabaseDialog({
  open,
  connectionId,
  driverType,
  onClose,
  onCreated,
}: CreateDatabaseDialogProps) {
  const isPg = driverType === "postgres" || driverType === "postgresql";
  const isMssql = driverType === "sqlserver" || driverType === "mssql";

  const [name, setName] = useState("");
  const [charset, setCharset] = useState("utf8mb4");
  const [collation, setCollation] = useState(""); // 空 = 服务器默认
  const [encoding, setEncoding] = useState("UTF8");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reset = () => {
    setName("");
    setCharset("utf8mb4");
    setCollation("");
    setEncoding("UTF8");
    setError(null);
  };

  const buildSql = (): string => {
    const q = quoteIdent(name.trim(), driverType);
    if (isPg) {
      return `CREATE DATABASE ${q} ENCODING '${encoding}';`;
    }
    if (isMssql) {
      // T-SQL：CREATE DATABASE [name]，排序规则可选
      let sql = `CREATE DATABASE ${q}`;
      if (collation) sql += ` COLLATE ${collation}`;
      return sql + ";";
    }
    let sql = `CREATE DATABASE ${q} DEFAULT CHARACTER SET ${charset}`;
    if (collation) sql += ` COLLATE ${collation}`;
    return sql + ";";
  };

  const handleSubmit = async () => {
    if (!name.trim()) {
      setError("请输入数据库名");
      return;
    }
    setLoading(true);
    setError(null);
    try {
      await tauriService.executeQuery(connectionId, buildSql());
      reset();
      onCreated();
      onClose();
    } catch (err) {
      setError(`创建失败: ${err}`);
    } finally {
      setLoading(false);
    }
  };

  const collationOptions = [
    { value: "", label: "（服务器默认）" },
    ...(MYSQL_COLLATIONS[charset] ?? []).map((c) => ({ value: c, label: c })),
  ];

  return (
    <Dialog
      open={open}
      onClose={() => {
        reset();
        onClose();
      }}
      title="新建数据库"
      footer={
        <>
          <Button
            variant="outline"
            onClick={() => {
              reset();
              onClose();
            }}
          >
            取消
          </Button>
          <Button onClick={handleSubmit} loading={loading}>
            创建
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        {error && <Alert variant="destructive">{error}</Alert>}

        <div className="space-y-1.5">
          <Label htmlFor="db-name">数据库名</Label>
          <Input
            id="db-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="my_database"
            autoFocus
            onKeyDown={(e) => {
              if (e.key === "Enter") handleSubmit();
            }}
          />
        </div>

        {isPg ? (
          <div className="space-y-1.5">
            <Label htmlFor="db-encoding">编码</Label>
            <Select
              id="db-encoding"
              value={encoding}
              onChange={(e) => setEncoding(e.target.value)}
              options={PG_ENCODINGS.map((e) => ({ value: e, label: e }))}
            />
          </div>
        ) : isMssql ? (
          <p className="text-xs text-muted-foreground">
            SQL Server 使用服务器默认字符集/排序规则。
          </p>
        ) : (
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <Label htmlFor="db-charset">字符集</Label>
              <Select
                id="db-charset"
                value={charset}
                onChange={(e) => {
                  setCharset(e.target.value);
                  setCollation("");
                }}
                options={MYSQL_CHARSETS.map((c) => ({ value: c, label: c }))}
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="db-collation">排序规则</Label>
              <Select
                id="db-collation"
                value={collation}
                onChange={(e) => setCollation(e.target.value)}
                options={collationOptions}
              />
            </div>
          </div>
        )}

        {name.trim() && (
          <div className="space-y-1.5">
            <Label>SQL 预览</Label>
            <pre className="rounded-md bg-muted px-3 py-2 text-xs font-mono whitespace-pre-wrap break-all">
              {buildSql()}
            </pre>
          </div>
        )}
      </div>
    </Dialog>
  );
}
