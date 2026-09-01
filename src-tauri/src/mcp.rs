//! SQLink 内置 MCP Streamable HTTP 服务。
//!
//! 服务只监听 127.0.0.1，使用每次启动随机生成的 Bearer Token，且仅暴露只读工具。

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{database::pool::ConnectionPoolManager, models::QueryResult};

const DEFAULT_PORT: u16 = 48123;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;
const MAX_RESPONSE_BYTES: usize = 1_000_000;
const QUERY_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConnectionRequest {
    #[schemars(description = "SQLink 连接 ID；可先调用 list_connections 获取")]
    connection_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DatabaseRequest {
    connection_id: String,
    #[schemars(description = "数据库名；SQLite 可省略")]
    database: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TableRequest {
    connection_id: String,
    database: Option<String>,
    table: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ScalarRequest {
    connection_id: String,
    database: Option<String>,
    #[schemars(description = "只读 SQL，必须只返回一行一列")]
    sql: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadRowsRequest {
    connection_id: String,
    database: Option<String>,
    table: String,
    #[schemars(description = "要读取的列名；省略或空数组表示全部列")]
    columns: Option<Vec<String>>,
    #[schemars(description = "从第几行开始，默认 0")]
    offset: Option<usize>,
    #[schemars(description = "读取行数，默认 100，最大 1000")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadonlySqlRequest {
    connection_id: String,
    database: Option<String>,
    #[schemars(description = "单条只读 SQL；禁止写入、DDL、事务和多语句")]
    sql: String,
    #[schemars(description = "最多返回行数，默认 100，最大 1000")]
    limit: Option<usize>,
}

#[derive(Clone)]
struct SqlinkMcp {
    manager: ConnectionPoolManager,
    tool_router: ToolRouter<Self>,
}

impl SqlinkMcp {
    fn new(manager: ConnectionPoolManager) -> Self {
        Self {
            manager,
            tool_router: Self::tool_router(),
        }
    }

    async fn query(
        &self,
        connection_id: &str,
        database: Option<&str>,
        sql: &str,
    ) -> Result<QueryResult, String> {
        let driver = self
            .manager
            .get_connection(connection_id)
            .await
            .map_err(|e| e.to_string())?;
        tokio::time::timeout(
            Duration::from_secs(QUERY_TIMEOUT_SECS),
            driver.query(sql, database, None),
        )
        .await
        .map_err(|_| format!("查询超过 {} 秒，已停止等待", QUERY_TIMEOUT_SECS))?
        .map_err(|e| e.to_string())
    }
}

#[tool_router]
impl SqlinkMcp {
    #[tool(description = "列出 SQLink 中已保存的数据库连接；不返回密码")]
    async fn list_connections(&self) -> Result<String, String> {
        let connections = self.manager.list_saved().map_err(|e| e.to_string())?;
        json_text(&connections)
    }

    #[tool(description = "列出指定连接中的数据库")]
    async fn list_databases(
        &self,
        Parameters(req): Parameters<ConnectionRequest>,
    ) -> Result<String, String> {
        let driver = self
            .manager
            .get_connection(&req.connection_id)
            .await
            .map_err(|e| e.to_string())?;
        let databases = driver.get_databases().await.map_err(|e| e.to_string())?;
        json_text(&databases)
    }

    #[tool(description = "列出指定数据库中的数据表及行数、引擎、注释等信息")]
    async fn list_tables(
        &self,
        Parameters(req): Parameters<DatabaseRequest>,
    ) -> Result<String, String> {
        let driver = self
            .manager
            .get_connection(&req.connection_id)
            .await
            .map_err(|e| e.to_string())?;
        let tables = driver
            .get_tables(req.database.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        json_text(&tables)
    }

    #[tool(description = "读取指定表的列、索引和外键结构")]
    async fn describe_table(
        &self,
        Parameters(req): Parameters<TableRequest>,
    ) -> Result<String, String> {
        let driver = self
            .manager
            .get_connection(&req.connection_id)
            .await
            .map_err(|e| e.to_string())?;
        let (columns, indexes, foreign_keys) = tokio::try_join!(
            driver.get_columns(req.database.as_deref(), &req.table),
            driver.get_indexes(req.database.as_deref(), &req.table),
            driver.get_foreign_keys(req.database.as_deref(), &req.table),
        )
        .map_err(|e| e.to_string())?;
        json_text(&json!({ "columns": columns, "indexes": indexes, "foreignKeys": foreign_keys }))
    }

    #[tool(description = "执行只读 SQL 并返回唯一的一项数值；SQL 必须恰好返回一行一列")]
    async fn query_scalar(
        &self,
        Parameters(req): Parameters<ScalarRequest>,
    ) -> Result<String, String> {
        validate_readonly_sql(&req.sql)?;
        let result = self
            .query(&req.connection_id, req.database.as_deref(), &req.sql)
            .await?;
        if result.columns.len() != 1 || result.rows.len() != 1 {
            return Err(format!(
                "query_scalar 要求一行一列，实际为 {} 行 {} 列",
                result.rows.len(),
                result.columns.len()
            ));
        }
        json_text(&json!({
            "column": result.columns[0].name,
            "value": result.rows[0].values[0],
            "executionTimeMs": result.execution_time_ms,
        }))
    }

    #[tool(description = "按 offset 和 limit 分段读取指定表的数据，单次最多 1000 行")]
    async fn read_table_rows(
        &self,
        Parameters(req): Parameters<ReadRowsRequest>,
    ) -> Result<String, String> {
        let driver = self
            .manager
            .get_connection(&req.connection_id)
            .await
            .map_err(|e| e.to_string())?;
        let limit = checked_limit(req.limit)?;
        let offset = req.offset.unwrap_or(0);
        let table = driver.escape_identifier(&req.table);
        let columns = match req.columns {
            Some(columns) if !columns.is_empty() => columns
                .iter()
                .map(|column| driver.escape_identifier(column))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "*".to_string(),
        };
        let sql = match driver.driver_type() {
            "sqlserver" => format!(
                "SELECT {} FROM {} ORDER BY (SELECT NULL) OFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
                columns, table, offset, limit
            ),
            _ => format!(
                "SELECT {} FROM {} LIMIT {} OFFSET {}",
                columns, table, limit, offset
            ),
        };
        let mut result = self
            .query(&req.connection_id, req.database.as_deref(), &sql)
            .await?;
        result.rows.truncate(limit);
        json_text(&result)
    }

    #[tool(description = "执行单条只读 SQL 并按数量返回结果；禁止写入、DDL、事务和多语句")]
    async fn execute_readonly_sql(
        &self,
        Parameters(req): Parameters<ReadonlySqlRequest>,
    ) -> Result<String, String> {
        validate_readonly_sql(&req.sql)?;
        let limit = checked_limit(req.limit)?;
        let mut result = self
            .query(&req.connection_id, req.database.as_deref(), &req.sql)
            .await?;
        let was_truncated = result.rows.len() > limit || result.truncated;
        result.rows.truncate(limit);
        result.truncated = was_truncated;
        json_text(&result)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SqlinkMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sqlink", env!("CARGO_PKG_VERSION")).with_title("SQLink MCP"))
            .with_instructions("SQLink 只读数据库工具。先调用 list_connections 获取 connection_id；所有查询均受行数、超时和响应大小限制。")
    }
}

#[derive(Clone)]
struct AuthToken(Arc<str>);

async fn authorize(
    State(expected): State<AuthToken>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let actual = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let expected = format!("Bearer {}", expected.0);
    if actual != Some(expected.as_str()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpClientConfig<'a> {
    transport: &'a str,
    url: String,
    authorization: String,
}

pub async fn start(manager: ConnectionPoolManager, config_dir: PathBuf) -> Result<(), String> {
    let port = std::env::var("SQLINK_MCP_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|e| format!("无法监听 {address}: {e}"))?;

    let config_path = config_dir.join("mcp-server.json");
    let token = load_or_create_token(&config_path);
    let endpoint = format!("http://{address}/mcp");
    let config = McpClientConfig {
        transport: "streamable-http",
        url: endpoint.clone(),
        authorization: format!("Bearer {token}"),
    };
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("写入 MCP 配置失败: {e}"))?;

    let cancellation = CancellationToken::new();
    let service: StreamableHttpService<SqlinkMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(SqlinkMcp::new(manager.clone())),
        Default::default(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .with_cancellation_token(cancellation.child_token()),
    );
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            AuthToken(token.into()),
            authorize,
        ));

    tracing::info!(url = %endpoint, config = %config_dir.join("mcp-server.json").display(), "MCP 服务已启动");
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

fn checked_limit(limit: Option<usize>) -> Result<usize, String> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(format!("limit 必须在 1 到 {MAX_LIMIT} 之间"));
    }
    Ok(limit)
}

/// Token 优先读取环境变量；否则复用上次配置，确保 MCP 客户端重启后仍可连接。
fn load_or_create_token(config_path: &std::path::Path) -> String {
    if let Ok(token) = std::env::var("SQLINK_MCP_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return token.to_string();
        }
    }
    if let Ok(content) = std::fs::read_to_string(config_path) {
        if let Ok(value) = serde_json::from_str::<Value>(&content) {
            let saved_token = value
                .get("authorization")
                .and_then(Value::as_str)
                .and_then(|value| value.strip_prefix("Bearer "))
                .filter(|value| !value.is_empty());
            if let Some(token) = saved_token {
                return token.to_string();
            }
        }
    }
    uuid::Uuid::new_v4().simple().to_string()
}

fn json_text<T: Serialize>(value: &T) -> Result<String, String> {
    let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
    if text.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "响应超过 {} 字节，请减小 limit 或列数",
            MAX_RESPONSE_BYTES
        ));
    }
    Ok(text)
}

/// 保守的只读 SQL 校验：忽略字符串和注释后，仅允许一条查询语句，并拒绝所有写操作关键字。
fn validate_readonly_sql(sql: &str) -> Result<(), String> {
    let normalized = strip_literals_and_comments(sql)?;
    let statements = normalized
        .split(';')
        .filter(|part| !part.trim().is_empty())
        .count();
    if statements != 1 {
        return Err("只允许执行一条 SQL".into());
    }
    let tokens: Vec<String> = normalized
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_uppercase())
        .collect();
    let first = tokens.first().map(String::as_str).unwrap_or("");
    const ALLOWED: &[&str] = &[
        "SELECT", "WITH", "SHOW", "DESC", "DESCRIBE", "EXPLAIN", "VALUES", "TABLE",
    ];
    const DENIED: &[&str] = &[
        "INSERT",
        "UPDATE",
        "DELETE",
        "MERGE",
        "REPLACE",
        "UPSERT",
        "CREATE",
        "ALTER",
        "DROP",
        "TRUNCATE",
        "GRANT",
        "REVOKE",
        "CALL",
        "EXEC",
        "EXECUTE",
        "COPY",
        "LOAD",
        "INTO",
        "ATTACH",
        "DETACH",
        "PRAGMA",
        "VACUUM",
        "ANALYZE",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "SAVEPOINT",
        "SET",
        "USE",
        "KILL",
        "SHUTDOWN",
    ];
    if !ALLOWED.contains(&first) {
        return Err(format!("不允许的 SQL 类型: {first}"));
    }
    if let Some(keyword) = tokens.iter().find(|token| DENIED.contains(&token.as_str())) {
        return Err(format!("只读模式禁止关键字: {keyword}"));
    }
    Ok(())
}

fn strip_literals_and_comments(sql: &str) -> Result<String, String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                let quote = chars[i];
                out.push(' ');
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == quote {
                        if i + 1 < chars.len() && chars[i + 1] == quote {
                            i += 2;
                            continue;
                        }
                        closed = true;
                        i += 1;
                        break;
                    }
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if !closed {
                    return Err("SQL 字符串未闭合".into());
                }
            }
            '-' if i + 1 < chars.len() && chars[i + 1] == '-' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                out.push(' ');
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                let mut closed = false;
                while i + 1 < chars.len() {
                    if chars[i] == '*' && chars[i + 1] == '/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return Err("SQL 块注释未闭合".into());
                }
                out.push(' ');
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"sqlink-test","version":"1.0"}}}"#;

    #[test]
    fn readonly_sql_allows_select_and_ignores_literals() {
        assert!(validate_readonly_sql("SELECT 'drop table x', id FROM users LIMIT 1").is_ok());
        assert!(validate_readonly_sql("WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
    }

    #[test]
    fn readonly_sql_rejects_writes_and_multiple_statements() {
        assert!(
            validate_readonly_sql("WITH x AS (DELETE FROM users RETURNING *) SELECT * FROM x")
                .is_err()
        );
        assert!(validate_readonly_sql("SELECT 1; SELECT 2").is_err());
        assert!(validate_readonly_sql("SELECT * INTO backup FROM users").is_err());
    }

    #[test]
    fn limit_is_bounded() {
        assert_eq!(checked_limit(None).unwrap(), DEFAULT_LIMIT);
        assert!(checked_limit(Some(0)).is_err());
        assert!(checked_limit(Some(MAX_LIMIT + 1)).is_err());
    }

    #[tokio::test]
    async fn mcp_http_requires_auth_and_exposes_tools() {
        let manager = ConnectionPoolManager::new(None);
        let cancellation = CancellationToken::new();
        let service: StreamableHttpService<SqlinkMcp, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(SqlinkMcp::new(manager.clone())),
                Default::default(),
                StreamableHttpServerConfig::default()
                    .with_stateful_mode(false)
                    .with_json_response(true)
                    .with_sse_keep_alive(None)
                    .with_cancellation_token(cancellation.child_token()),
            );
        let app =
            Router::new()
                .nest_service("/mcp", service)
                .layer(middleware::from_fn_with_state(
                    AuthToken("test-token".into()),
                    authorize,
                ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!("http://{address}/mcp");
        let unauthorized = client.post(&url).body(INIT_BODY).send().await.unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

        let initialized: Value = client
            .post(&url)
            .header("Authorization", "Bearer test-token")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(INIT_BODY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(initialized["result"]["serverInfo"]["name"], "sqlink");

        let tools: Value = client
            .post(&url)
            .header("Authorization", "Bearer test-token")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":{}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let names = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"list_connections"));
        assert!(names.contains(&"describe_table"));
        assert!(names.contains(&"read_table_rows"));
        assert!(names.contains(&"execute_readonly_sql"));
    }
}
