# SQLink MCP 工具

> 本文档面向需要让 AI 客户端只读访问 SQLink 已保存数据库连接的使用者和维护者。桌面 IPC、项目交接和构建发布分别见[《对接文档》](对接文档.md)、[《项目交接文档》](项目交接文档.md)和[《构建与发布操作手册》](构建与发布操作手册.md)。

## 1. 服务概览

SQLink 桌面应用启动时会异步启动一个本机 MCP Streamable HTTP 服务。服务复用 SQLink 保存的 MySQL/MariaDB、PostgreSQL、SQLite 和 SQL Server 连接，提供连接发现、Schema 读取和受限制的只读查询。

| 项目 | 默认值 |
| --- | --- |
| 地址 | `http://127.0.0.1:48123/mcp` |
| 传输 | Streamable HTTP |
| 会话模式 | 无状态 |
| 返回模式 | JSON response |
| 鉴权 | `Authorization: Bearer <token>` |
| 默认行数 | 100 |
| 最大行数 | 1000 |
| 查询等待上限 | 30 秒 |
| 最大序列化响应 | 1,000,000 字节 |

MCP 启动失败不会阻止桌面主程序启动。使用 MCP 前必须保持 SQLink 正在运行。

## 1.1 应用内设置

SQLink 主窗口顶栏的「MCP 设置」可管理服务，保存后立即生效：

- **启用开关**：停用后服务不再监听任何端口；重新启用自动启动。
- **端口**：修改保存后自动重启服务，无需重启应用；`mcp-server.json` 的 URL 同步更新（Token 不变）。
- **工具开关**：被禁用的工具不出现在 `tools/list`，调用直接报错。
- **连接白名单**：未勾选允许的连接对 MCP 客户端不可见，直接访问返回未授权错误。

设置持久化在 `mcp-settings.json`（见下节）；`SQLINK_MCP_PORT` / `SQLINK_MCP_TOKEN` 环境变量仍优先于设置文件。

## 2. 配置文件

启动成功后，应用会在 Tauri `app_config_dir` 下写入：

```text
mcp-server.json
```

内容示例：

```json
{
  "transport": "streamable-http",
  "url": "http://127.0.0.1:48123/mcp",
  "authorization": "Bearer <令牌>"
}
```

令牌处理规则：

1. `SQLINK_MCP_TOKEN` 非空时优先使用该值。
2. 否则尝试复用现有 `mcp-server.json` 中的 Bearer Token。
3. 没有可复用令牌时生成新的 UUID Token。

因此令牌通常会在应用重启后保持不变，并非每次启动都刷新。`mcp-server.json` 含有效访问令牌，应按敏感配置文件保护，不要提交到 Git、工单或公开聊天。

`mcp-settings.json` 是 MCP 服务自身的设置（与上文的客户端接入文件相互独立）：

```json
{
  "enabled": true,
  "port": 48123,
  "disabledTools": []
}
```

- `enabled` 为 `false` 时服务不启动。
- `port` 为默认监听端口；`SQLINK_MCP_PORT` 环境变量优先于该值。
- `disabledTools` 中的工具不会出现在 `tools/list`。
- 推荐通过应用内「MCP 设置」修改；手工编辑本文件需重启 SQLink 才生效。文件缺失或损坏时按默认值处理（启用、48123、无禁用工具）。

## 3. 客户端接入

不同 MCP 客户端的配置字段名称不同，核心信息只有两项：

- URL：读取 `mcp-server.json` 的 `url`。
- 请求头：读取 `authorization`，作为 HTTP `Authorization` Header 原样发送。

通用概念示例：

```json
{
  "url": "http://127.0.0.1:48123/mcp",
  "headers": {
    "Authorization": "Bearer <令牌>"
  }
}
```

不要把此示例直接视为所有客户端都支持的完整配置；应按照目标客户端的 Streamable HTTP MCP 配置格式映射。

推荐接入顺序：

1. 在 SQLink 中保存数据库连接，并选择保存密码。
2. 确认桌面应用正在运行。
3. 读取当前 `mcp-server.json`。
4. 在 MCP 客户端配置 URL 和 Authorization Header。
5. 调用 `list_connections` 验证连接。
6. 依次调用 `list_databases`、`list_tables` 和 `describe_table`。
7. 确认字段后再读取表数据或执行只读 SQL。

如果连接没有保存密码，MCP 在活动连接不存在时可能无法自动重连。

## 4. 工具清单

以下工具均可在应用内「MCP 设置」中逐一禁用；被禁用的工具不会出现在 `tools/list`，调用返回错误。

### 4.1 list_connections

用途：列出 SQLink 中已保存的数据库连接；未在 MCP 设置中勾选允许的连接不会出现在返回结果中。

- 参数：无。
- 返回：JSON 字符串形式的 `ConnectionConfig[]`。
- 安全：密码字段为空，不返回明文或密文。

后续工具需要使用返回记录中的 `id` 作为 `connection_id`。

### 4.2 list_databases

用途：列出指定连接中的数据库。

参数：

```json
{
  "connection_id": "<连接 ID>"
}
```

返回数据库名称数组。若当前无活动连接，后端会尝试使用已保存配置自动连接。

### 4.3 list_tables

用途：列出数据库中的数据表，以及驱动能够提供的行数、引擎、Schema 和注释信息。

参数：

```json
{
  "connection_id": "<连接 ID>",
  "database": "<数据库名称，可选>"
}
```

### 4.4 describe_table

用途：并行读取指定表的列、索引和外键。

参数：

```json
{
  "connection_id": "<连接 ID>",
  "database": "<数据库名称，可选>",
  "table": "<表名>"
}
```

返回结构：

```json
{
  "columns": [],
  "indexes": [],
  "foreignKeys": []
}
```

三个元数据查询中任意一个失败时，整个工具调用失败。

### 4.5 query_scalar

用途：执行只读 SQL，并要求结果恰好为一行一列。

参数：

```json
{
  "connection_id": "<连接 ID>",
  "database": "<数据库名称，可选>",
  "sql": "SELECT COUNT(*) FROM users"
}
```

如果不是一行一列，工具返回错误。适合计数、最大值、状态值等查询。

### 4.6 read_table_rows

用途：使用 `offset + limit` 对指定表分页读取，可选部分列。

参数：

```json
{
  "connection_id": "<连接 ID>",
  "database": "<数据库名称，可选>",
  "table": "<表名>",
  "columns": ["id", "name"],
  "offset": 0,
  "limit": 100
}
```

- `columns` 为空或缺省时读取全部列。
- `offset` 缺省为 0。
- `limit` 缺省为 100，范围必须为 1–1000。
- 表名和列名由驱动按对应方言转义。
- SQL Server 使用 `OFFSET ... FETCH NEXT`，其他驱动使用 `LIMIT ... OFFSET`。

工具未要求稳定排序键；对持续变化的数据或需要可重复分页的场景，建议改用带明确 `ORDER BY` 的 `execute_readonly_sql`。

### 4.7 execute_readonly_sql

用途：执行一条自定义只读 SQL，并限制返回行数。

参数：

```json
{
  "connection_id": "<连接 ID>",
  "database": "<数据库名称，可选>",
  "sql": "SELECT id, name FROM users ORDER BY id",
  "limit": 100
}
```

- `limit` 缺省为 100，范围必须为 1–1000。
- 返回行超过 limit 时截断，并设置 `truncated`。
- 仍受驱动自身查询结果上限、30 秒等待上限和 1 MB 响应上限约束。
- limit 是返回裁剪，不一定会自动改写 SQL；大表查询仍应在 SQL 中写入合适的过滤和数据库方言限制。

## 5. 只读 SQL 规则

校验会先去除字符串和注释，再检查语句数和关键字。

允许作为首关键字：

```text
SELECT, WITH, SHOW, DESC, DESCRIBE, EXPLAIN, VALUES, TABLE
```

明确拒绝的关键字包括但不限于：

```text
INSERT, UPDATE, DELETE, MERGE, REPLACE, UPSERT,
CREATE, ALTER, DROP, TRUNCATE,
GRANT, REVOKE, CALL, EXEC, EXECUTE,
COPY, LOAD, INTO, ATTACH, DETACH, PRAGMA, VACUUM, ANALYZE,
BEGIN, COMMIT, ROLLBACK, SAVEPOINT, SET, USE, KILL, SHUTDOWN
```

同时拒绝：

- 空 SQL。
- 多条 SQL。
- 未闭合的字符串。
- 未闭合的块注释。
- 允许查询中嵌入被禁止关键字的写入 CTE。

校验是保守的词法防线，不是完整 SQL 语法解析或数据库沙箱。生产环境必须为 MCP 单独配置数据库只读账号，从权限层禁止写入、DDL 和敏感对象访问。

## 6. 安全边界

- 服务固定监听 `127.0.0.1`，不会主动暴露到局域网。
- 所有请求必须携带完全匹配的 Bearer Token，否则返回 HTTP `401`。
- Token 明文保存在本机配置文件中；能读取该文件的同用户进程可以使用 MCP。
- 连接列表不会返回密码，但工具会在后端复用保存的凭据。
- 标识符由数据库驱动转义。
- 只读 SQL 受到语句、关键字、行数、等待时间和响应大小限制。
- 仅「MCP 设置」中勾选允许的连接可被访问；未保存的活动连接始终拒绝。
- 30 秒超时表示服务停止等待；是否真正取消数据库端执行取决于底层驱动，不应把 HTTP 超时等同于数据库查询已经终止。
- MCP 启动失败不影响桌面应用，因此“SQLink 能打开”不代表“MCP 一定已启动”。

## 7. 环境变量

| 变量 | 作用 | 默认行为 |
| --- | --- | --- |
| `SQLINK_MCP_PORT` | 修改监听端口 | 设置界面端口，缺省 `48123`；非合法 `u16` 时回退默认值。环境变量优先于设置文件 |
| `SQLINK_MCP_TOKEN` | 指定固定 Token | 空值被忽略；否则优先于配置文件 |
| `SQLINK_LOG` | 调整日志级别 | `info,sqlconnect=debug` |

设置环境变量后必须从同一环境重新启动 SQLink 进程。

## 8. 故障排查

| 现象 | 检查 | 处理 |
| --- | --- | --- |
| 无法连接 MCP | SQLink 是否运行、配置文件是否生成 | 先启动应用并查看日志 |
| 连接被拒绝 | 端口是否监听、是否被其他程序占用 | 查看日志；设置新的 `SQLINK_MCP_PORT` 后重启 |
| HTTP 401 | Header 或 Token 不一致 | 重新读取当前 `mcp-server.json`，不要手抄旧 Token |
| `list_connections` 为空 | SQLink 中没有保存连接 | 先在桌面应用保存连接 |
| 工具提示连接已关闭 | 未保存密码、密钥损坏或数据库不可达 | 在桌面应用重新输入密码并保存，测试连接 |
| SQL 被拒绝 | 多语句或包含禁止关键字 | 改为单条纯只读查询，不要绕过安全检查 |
| limit 错误 | limit 为 0 或大于 1000 | 调整到 1–1000 |
| 响应超过限制 | 行数或列内容过大 | 减少 limit、选择少量列、增加 WHERE |
| 查询超过 30 秒 | SQL 慢、锁等待或网络问题 | 优化 SQL、检查数据库锁和日志；必要时在数据库端终止 |
| 桌面正常但 MCP 不工作 | MCP 异步启动失败 | 查看 SQLink 日志中的“MCP 服务启动失败” |
| `tools/list` 缺少某个工具 | MCP 设置中是否禁用了该工具 | 在「MCP 设置」中重新启用 |
| 工具返回"未允许 MCP 访问" | 连接是否在 MCP 设置中勾选允许 | 勾选该连接，或改用已允许的连接 |
| 设置的端口未生效 | 是否设置了 `SQLINK_MCP_PORT` | 环境变量优先于设置文件；取消后重启应用 |

## 9. 验收清单

- [ ] 未带 Authorization 请求返回 401。
- [ ] 正确 Token 可以完成 MCP initialize 和 tools/list。
- [ ] 工具列表包含 7 个预期工具。
- [ ] `list_connections` 不返回密码。
- [ ] `describe_table` 能返回列、索引和外键键名。
- [ ] 一行一列查询可通过 `query_scalar`。
- [ ] 多行或多列传给 `query_scalar` 会报错。
- [ ] 写 SQL、多语句和危险关键字被拒绝。
- [ ] limit 的 0、1000、1001 边界符合预期。
- [ ] 错误 Token、端口占用、大响应和超时路径有明确错误。
- [ ] 生产接入使用数据库只读账号。

## 10. 维护入口

- 服务实现和参数模型：`src-tauri/src/mcp.rs`。
- 连接复用：`src-tauri/src/database/pool.rs`。
- 驱动标识符转义：`src-tauri/src/database/*.rs`。
- 应用启动：`src-tauri/src/lib.rs`。
- 自动化测试：`src-tauri/src/mcp.rs` 中的 `#[cfg(test)]` 模块。

修改工具名、参数、鉴权、端口、限制、SQL 规则或配置文件结构时，必须同步更新本文档和相关测试。
