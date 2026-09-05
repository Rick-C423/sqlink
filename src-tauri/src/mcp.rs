//! SQLink 内置 MCP Streamable HTTP 服务。
//!
//! 服务只监听 127.0.0.1，使用每次启动随机生成的 Bearer Token，且仅暴露只读工具。

use std::{net::SocketAddr, sync::{Arc, Mutex}, time::Duration};

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

use crate::{
    database::pool::ConnectionPoolManager,
    mcp_settings::{McpSettings, McpSettingsState},
    models::QueryResult,
};

/// 全部 MCP 工具名；设置界面的工具开关以此为准。
pub const MCP_TOOLS: [&str; 7] = [
    "list_connections",
    "list_databases",
    "list_tables",
    "describe_table",
    "query_scalar",
    "read_table_rows",
    "execute_readonly_sql",
];

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
    fn new(manager: ConnectionPoolManager, settings: Arc<McpSettings>) -> Self {
        let mut tool_router = Self::tool_router();
        // 每请求新建实例（stateless 模式），因此工具开关的变更即时生效，无需重启服务
        for name in &settings.disabled_tools {
            tool_router.disable_route(name.clone());
        }
        Self {
            manager,
            tool_router,
        }
    }

    /// 连接白名单：仅已保存且未被关闭 MCP 访问的连接可用（mcpAllowed 缺省视为允许）。
    fn connection_allowed(&self, connection_id: &str) -> Result<(), String> {
        let saved = self.manager.list_saved().map_err(|e| e.to_string())?;
        match saved.iter().find(|c| c.id.as_deref() == Some(connection_id)) {
            Some(config) if config.mcp_allowed != Some(false) => Ok(()),
            Some(_) => Err("该连接未允许 MCP 访问：请在 SQLink 的 MCP 设置中开启".into()),
            None => Err("连接不存在或未在 SQLink 中保存".into()),
        }
    }

    /// 已保存连接中允许 MCP 访问的部分
    fn allowed_connections(&self) -> Result<Vec<crate::models::ConnectionConfig>, String> {
        let saved = self.manager.list_saved().map_err(|e| e.to_string())?;
        Ok(saved
            .into_iter()
            .filter(|c| c.mcp_allowed != Some(false))
            .collect())
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
        let connections = self.allowed_connections()?;
        json_text(&connections)
    }

    #[tool(description = "列出指定连接中的数据库")]
    async fn list_databases(
        &self,
        Parameters(req): Parameters<ConnectionRequest>,
    ) -> Result<String, String> {
        self.connection_allowed(&req.connection_id)?;
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
        self.connection_allowed(&req.connection_id)?;
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
        self.connection_allowed(&req.connection_id)?;
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
        self.connection_allowed(&req.connection_id)?;
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
        self.connection_allowed(&req.connection_id)?;
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
        self.connection_allowed(&req.connection_id)?;
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

/// MCP 服务的运行状态（给设置界面的状态展示）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRunStatus {
    pub running: bool,
    pub url: Option<String>,
    pub port: Option<u16>,
}

struct RunningService {
    handle: tauri::async_runtime::JoinHandle<()>,
    token: CancellationToken,
    port: u16,
}

/// MCP 服务生命周期管理（Tauri managed state）：
/// 按设置启动/停止/重启服务；端口占用等启动错误会返回给设置界面。
#[derive(Clone, Default)]
pub struct McpSupervisor {
    inner: Arc<Mutex<Option<RunningService>>>,
}

impl McpSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前运行状态；不在运行时 running=false。
    pub fn status(&self) -> McpRunStatus {
        let guard = self.inner.lock().expect("MCP 状态锁不会中毒");
        match guard.as_ref() {
            Some(running) => McpRunStatus {
                running: true,
                url: Some(format!("http://127.0.0.1:{}/mcp", running.port)),
                port: Some(running.port),
            },
            None => McpRunStatus {
                running: false,
                url: None,
                port: None,
            },
        }
    }

    /// 优雅停止当前服务并等待端口释放。
    pub async fn stop(&self) {
        let running = self.inner.lock().expect("MCP 状态锁不会中毒").take();
        if let Some(running) = running {
            running.token.cancel();
            let _ = running.handle.await;
            tracing::info!("MCP 服务已停止");
        }
    }

    /// 按设置把服务调整到目标状态：停用 → 停止；启用 → 旧实例停止后在新端口启动。
    /// 绑定失败时返回错误，此时服务处于停止状态，可修正设置后重试。
    pub async fn apply(
        &self,
        settings: &McpSettings,
        settings_state: &McpSettingsState,
        manager: &ConnectionPoolManager,
    ) -> Result<McpRunStatus, String> {
        self.stop().await;
        if !settings.enabled {
            return Ok(self.status());
        }
        self.start_service(settings, settings_state, manager).await?;
        Ok(self.status())
    }

    async fn start_service(
        &self,
        settings: &McpSettings,
        settings_state: &McpSettingsState,
        manager: &ConnectionPoolManager,
    ) -> Result<(), String> {
        // 环境变量显式指定端口时优先于设置文件（与历史行为一致）
        let port = std::env::var("SQLINK_MCP_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(settings.port);
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|e| format!("无法监听 {address}: {e}"))?;

        let config_dir = settings_state.config_dir().to_path_buf();
        let config_path = config_dir.join("mcp-server.json");
        let token_value = load_or_create_token(&config_path);
        let endpoint = format!("http://{address}/mcp");
        let config = McpClientConfig {
            transport: "streamable-http",
            url: endpoint.clone(),
            authorization: format!("Bearer {token_value}"),
        };
        std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
        std::fs::write(
            &config_path,
            serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("写入 MCP 配置失败: {e}"))?;

        let cancellation = CancellationToken::new();
        let service: StreamableHttpService<SqlinkMcp, LocalSessionManager> =
            StreamableHttpService::new(
                {
                    let manager = manager.clone();
                    let settings_state = settings_state.clone();
                    move || Ok(SqlinkMcp::new(manager.clone(), settings_state.get()))
                },
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
                AuthToken(token_value.into()),
                authorize,
            ));

        let serve_token = cancellation.clone();
        let handle = tauri::async_runtime::spawn(async move {
            if let Err(error) = axum::serve(listener, app)
                .with_graceful_shutdown(serve_token.cancelled_owned())
                .await
            {
                tracing::error!(error = %error, "MCP 服务异常退出");
            }
        });
        *self.inner.lock().expect("MCP 状态锁不会中毒") = Some(RunningService {
            handle,
            token: cancellation,
            port,
        });

        tracing::info!(url = %endpoint, config = %config_dir.join("mcp-server.json").display(), "MCP 服务已启动");
        Ok(())
    }
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
    use crate::models::ConnectionConfig;
    use crate::store::ConnectionStore;

    const INIT_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"sqlink-test","version":"1.0"}}}"#;

    fn accept_headers() -> reqwest::header::HeaderMap {
        use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers
    }

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

    #[test]
    fn whitelist_filters_denied_and_unsaved_connections() {
        let dir = std::env::temp_dir().join(format!(
            "sqlink-mcp-whitelist-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = ConnectionStore::new(dir.clone()).unwrap();
        let config = |id: &str, allowed: Option<bool>| ConnectionConfig {
            id: Some(id.into()),
            name: id.into(),
            driver_type: "sqlite".into(),
            host: String::new(),
            port: 0,
            user: String::new(),
            password: String::new(),
            database: Some(dir.join("t.db").to_string_lossy().into_owned()),
            encrypt: None,
            save_password: None,
            group_name: None,
            color: None,
            mcp_allowed: allowed,
        };
        store.save(&config("allow-default", None)).unwrap();
        store.save(&config("allow-true", Some(true))).unwrap();
        store.save(&config("deny", Some(false))).unwrap();

        let manager = ConnectionPoolManager::new(Some(dir.clone()));
        let mcp = SqlinkMcp::new(manager, Arc::new(McpSettings::default()));

        let list = mcp.allowed_connections().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|c| c.id.as_deref() != Some("deny")));

        assert!(mcp.connection_allowed("allow-default").is_ok());
        assert!(mcp.connection_allowed("allow-true").is_ok());
        assert!(mcp.connection_allowed("deny").unwrap_err().contains("未允许"));
        assert!(mcp.connection_allowed("missing").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn mcp_http_requires_auth_and_exposes_tools() {
        let manager = ConnectionPoolManager::new(None);
        let cancellation = CancellationToken::new();
        let service: StreamableHttpService<SqlinkMcp, LocalSessionManager> =
            StreamableHttpService::new(
                move || {
                    Ok(SqlinkMcp::new(
                        manager.clone(),
                        Arc::new(McpSettings::default()),
                    ))
                },
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
            .headers(accept_headers())
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
            .headers(accept_headers())
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

    #[tokio::test]
    async fn mcp_disabled_tools_hidden_and_rejected() {
        let manager = ConnectionPoolManager::new(None);
        let settings = Arc::new(McpSettings {
            enabled: true,
            port: 0,
            disabled_tools: vec!["execute_readonly_sql".into(), "read_table_rows".into()],
        });
        let cancellation = CancellationToken::new();
        let service: StreamableHttpService<SqlinkMcp, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(SqlinkMcp::new(manager.clone(), settings.clone())),
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
        let headers = accept_headers();
        let auth = reqwest::header::HeaderValue::from_static("Bearer test-token");

        let tools: Value = client
            .post(&url)
            .header("Authorization", auth.clone())
            .headers(headers.clone())
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
        assert!(!names.contains(&"execute_readonly_sql"));
        assert!(!names.contains(&"read_table_rows"));
        assert!(names.contains(&"list_connections"));
        assert!(names.contains(&"describe_table"));

        // 被禁用的工具调用直接报错
        let call: Value = client
            .post(&url)
            .header("Authorization", auth)
            .headers(headers)
            .json(&json!({
                "jsonrpc":"2.0", "id":3, "method":"tools/call",
                "params":{"name":"execute_readonly_sql","arguments":{"sql":"SELECT 1"}}
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(call.get("error").is_some(), "disabled tool call should error: {call}");
    }
}
