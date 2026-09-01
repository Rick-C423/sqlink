# SQLink MCP 工具

SQLink 启动后会在本机同时启动一个只读 MCP Streamable HTTP 服务，AI 客户端可以复用 SQLink 保存的数据库连接读取库、表、结构和数据。

## 连接信息

- 默认地址：`http://127.0.0.1:48123/mcp`
- 传输协议：Streamable HTTP
- 鉴权：`Authorization: Bearer <token>`
- 配置文件：SQLink 应用配置目录下的 `mcp-server.json`

`mcp-server.json` 会在每次 SQLink 启动时刷新，示例：

```json
{
  "transport": "streamable-http",
  "url": "http://127.0.0.1:48123/mcp",
  "authorization": "Bearer <本次启动生成的随机令牌>"
}
```

必须先启动 SQLink，MCP 客户端才能连接。端口被占用时服务不会启动，错误会写入 SQLink 日志。开发或部署时可以通过环境变量 `SQLINK_MCP_PORT` 修改端口。

## 工具清单

| 工具 | 用途 |
| --- | --- |
| `list_connections` | 列出已保存连接，不包含密码 |
| `list_databases` | 获取指定连接的数据库列表 |
| `list_tables` | 获取指定数据库的表列表 |
| `describe_table` | 获取表的列、索引和外键 |
| `query_scalar` | 执行只读 SQL，返回唯一的一行一列值 |
| `read_table_rows` | 按 `offset + limit` 分段读取表数据 |
| `execute_readonly_sql` | 执行单条只读 SQL并限量返回结果 |

推荐调用顺序：

1. 调用 `list_connections` 获取 `connection_id`。
2. 调用 `list_databases` 和 `list_tables` 定位目标表。
3. 使用 `describe_table` 了解字段后，再读取数据或执行只读查询。

## 安全限制

- 服务只监听 `127.0.0.1`，不会暴露到局域网。
- 首次启动生成 Bearer Token，后续启动自动复用；也可通过 `SQLINK_MCP_TOKEN` 环境变量指定。未携带正确 Token 的请求返回 `401`。
- SQL 工具只接受单条只读语句，拒绝写入、DDL、事务、存储过程调用和多语句。
- `read_table_rows` 和 `execute_readonly_sql` 默认返回 100 行，单次最多 1000 行。
- 查询等待时间最多 30 秒，单次序列化响应最多 1 MB。
- 表名和列名由数据库驱动按对应方言转义，不直接拼接未经处理的标识符。

> MCP 的只读校验是额外防线。生产环境仍建议为 AI 单独配置数据库只读账号，从数据库权限层彻底禁止写操作。
