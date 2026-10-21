//! 显式启用的真实网络数据库测试；仅使用 SQLINK_TEST_* 指定的测试库。
//! 连接串格式为 driver://user:password@host:port/database，凭据可用百分号编码。
//! MSSQL 可追加 ?encrypt=true；未配置环境变量时显式运行必须失败。
//! 每次只创建并清理自己的随机表，不复用已有表，不改变全局写入策略。

use reqwest::Url;
use serde_json::json;
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::result_context::{ResultContext, RowIdentityValue};
use sqlconnect_lib::table_result::{
    self, ContextReference, ContextValidationRequest, TablePageRequest,
};
use uuid::Uuid;

const EXACT_KEY: &str = "9007199254740993";
const KEY_COLUMN: &str = "record_key";

type TestResult<T = ()> = Result<T, String>;

// 校验失败通过 Result 退出，保证外围先清理测试表和连接再报告失败。
fn require(condition: bool, message: &str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn decode_component(encoded: &str) -> TestResult<String> {
    let mut decoded = Vec::new();
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => decoded.push((high * 16 + low) as u8),
                _ => return Err("连接串含非法百分号编码".into()),
            }
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).map_err(|_| "连接串字段不是有效 UTF-8".into())
}

fn config_from_env(driver: &str, variable: &str, id: &str) -> TestResult<ConnectionConfig> {
    let raw =
        std::env::var(variable).map_err(|_| format!("必须设置 {variable} 才能显式运行此测试"))?;
    // 不输出原始连接串或 URL 解析错误，避免失败日志泄露凭据。
    let url = Url::parse(&raw).map_err(|_| format!("{variable} 不是有效测试连接串"))?;
    let valid_scheme = match driver {
        "postgres" => matches!(url.scheme(), "postgres" | "postgresql"),
        "mssql" => matches!(url.scheme(), "mssql" | "sqlserver"),
        _ => url.scheme() == driver,
    };
    require(valid_scheme, "测试连接串协议与驱动不符")?;
    require(url.fragment().is_none(), "测试连接串不支持片段")?;
    let host = url.host_str().ok_or("测试连接串缺少主机")?;
    let database = decode_component(url.path().trim_start_matches('/'))?;
    require(!database.is_empty(), "测试连接串必须显式指定数据库")?;
    let mut encrypt = None;
    for (name, value) in url.query_pairs() {
        match (driver, name.as_ref(), value.as_ref()) {
            ("mssql", "encrypt", "true") => encrypt = Some(true),
            ("mssql", "encrypt", "false") => encrypt = Some(false),
            _ => return Err("测试连接串包含不支持的参数".into()),
        }
    }
    Ok(ConnectionConfig {
        id: Some(id.into()),
        name: format!("受控结果网络测试 {driver}"),
        driver_type: driver.into(),
        host: host.trim_start_matches('[').trim_end_matches(']').into(),
        port: url.port().unwrap_or(match driver {
            "postgres" => 5432,
            "mssql" => 1433,
            _ => 3306,
        }),
        user: decode_component(url.username())?,
        password: decode_component(url.password().unwrap_or_default())?,
        database: Some(database),
        encrypt,
        save_password: Some(false),
        group_name: None,
        color: None,
        mcp_allowed: Some(false),
    })
}

fn request(id: &str, database: &str, parts: &[String]) -> TablePageRequest {
    TablePageRequest {
        protocol_version: 1,
        connection_id: id.into(),
        database: database.into(),
        table_parts: parts.to_vec(),
        page: 1,
        page_size: 20,
        sort: None,
    }
}

fn reference(context: &ResultContext, handle: &str) -> ContextValidationRequest {
    ContextValidationRequest {
        protocol_version: 1,
        result_id: context.result_id.clone(),
        revision: context.revision,
        row_handles: vec![handle.into()],
    }
}

async fn execute(
    manager: &ConnectionPoolManager,
    id: &str,
    database: &str,
    sql: &str,
    stage: &str,
) -> TestResult {
    let driver = manager
        .get_connection(id)
        .await
        .map_err(|_| format!("{stage}：连接不可用"))?;
    driver
        .execute(sql, Some(database), None)
        .await
        .map_err(|_| format!("{stage}失败"))?;
    Ok(())
}

async fn check_contexts(
    manager: &ConnectionPoolManager,
    id: &str,
    database: &str,
    table: &str,
    parts: &[String],
) -> TestResult {
    let mut first = table_result::read_table_page(manager, request(id, database, parts))
        .await
        .map_err(|error| format!("首次受控读取失败：{}", error.code))?;
    require(
        first.context.source.kind == "table",
        "真实目录来源必须是 table",
    )?;
    require(
        first.table.object_kind.as_deref() == Some("table"),
        "解析结果必须是真实表",
    )?;
    require(first.context.source.table == table, "来源必须是本次新建表")?;
    require(
        first.context.source.database == first.table.database,
        "来源数据库必须匹配真实目录",
    )?;
    require(
        first.context.source.schema == first.table.schema,
        "来源 schema 必须匹配真实目录",
    )?;
    require(first.total_rows.is_none(), "受控读取不应同步统计整表")?;
    require(first.context.columns.len() == 2, "两列都应具有真实来源")?;
    for column in &first.context.columns {
        let origin = column.origin.as_ref().ok_or("受控列缺少来源")?;
        require(
            origin.database == first.context.source.database
                && origin.schema == first.context.source.schema
                && origin.table == table
                && origin.column == column.display_name,
            "列来源必须逐项匹配真实目录，不能按列名猜测表来源",
        )?;
    }
    let key_index = first
        .table
        .columns
        .iter()
        .position(|column| column.name == KEY_COLUMN)
        .ok_or("真实列元数据缺少测试主键")?;
    require(
        first.table.columns[key_index].is_primary_key,
        "必须由目录确认主键身份",
    )?;
    require(first.result.rows.len() == 1, "测试表应返回唯一一行")?;
    require(
        first.result.rows[0].values.get(key_index) == Some(&Some(json!(EXACT_KEY))),
        "大整数显示必须保留完整十进制字符串",
    )?;
    let plain_index = first
        .table
        .columns
        .iter()
        .position(|column| column.name == "plain_value")
        .ok_or("真实列元数据缺少普通测试列")?;
    require(
        first.result.rows[0].values.get(plain_index) == Some(&Some(json!("context fixture"))),
        "普通列读取值必须与夹具一致",
    )?;
    let handle = first
        .row_handles
        .first()
        .and_then(|handle| handle.clone())
        .ok_or("真实 bigint 主键必须获得 opaque handle")?;
    require(
        Uuid::parse_str(&handle).is_ok() && !handle.contains(EXACT_KEY),
        "行引用必须是不透明随机值",
    )?;
    table_result::validate_context(manager, reference(&first.context, &handle))
        .await
        .map_err(|error| format!("首次上下文校验失败：{}", error.code))?;
    // 显示值被修改也不能改变后端保存的定位键。
    first.result.rows[0].values[key_index] = Some(json!(0));
    let identities = manager
        .validate_result_handles(&first.context, &[handle.clone()])
        .await
        .map_err(|error| format!("后端行引用校验失败：{}", error.code))?;
    require(identities.len() == 1, "后端应保存唯一行身份")?;
    require(
        identities[0].keys
            == vec![(
                KEY_COLUMN.into(),
                RowIdentityValue::Integer(EXACT_KEY.into()),
            )],
        "后端主键必须精确保留 9007199254740993",
    )?;
    let second = table_result::read_table_page(manager, request(id, database, parts))
        .await
        .map_err(|error| format!("第二次受控读取失败：{}", error.code))?;
    require(
        first.context.result_id != second.context.result_id,
        "两次结果必须具有独立上下文",
    )?;
    let second_handle = second
        .row_handles
        .first()
        .and_then(|handle| handle.clone())
        .ok_or("第二次结果缺少行引用")?;
    table_result::validate_context(manager, reference(&second.context, &second_handle))
        .await
        .map_err(|error| format!("第二次上下文校验失败：{}", error.code))?;
    let cross_result =
        table_result::validate_context(manager, reference(&second.context, &handle)).await;
    require(
        matches!(cross_result, Err(error) if error.code == "INVALID_ROW_HANDLE"),
        "同连接不同结果不得复用旧行引用",
    )?;
    for (context, own_handle) in [(&first.context, &handle), (&second.context, &second_handle)] {
        table_result::release_context(
            manager,
            ContextReference {
                protocol_version: 1,
                result_id: context.result_id.clone(),
                revision: context.revision,
            },
        )
        .map_err(|error| format!("释放上下文失败：{}", error.code))?;
        let released =
            table_result::validate_context(manager, reference(context, own_handle)).await;
        require(
            matches!(released, Err(error) if error.code == "RESULT_CONTEXT_NOT_FOUND"),
            "释放后上下文必须失效",
        )?;
        require(
            manager
                .validate_result_handles(context, &[own_handle.to_string()])
                .await
                .is_err(),
            "释放后后端行引用必须失效",
        )?;
    }
    Ok(())
}

async fn exercise(driver: &str, variable: &str) -> TestResult {
    let suffix = Uuid::new_v4().simple().to_string();
    let id = format!("network_context_{suffix}");
    let table = format!("sqlink_context_{suffix}");
    let config = config_from_env(driver, variable, &id)?;
    let database = config.database.clone().ok_or("测试数据库未配置")?;
    let (qualified, parts) = match driver {
        "postgres" => (
            format!("public.{table}"),
            vec!["public".into(), table.clone()],
        ),
        "mssql" => (format!("dbo.{table}"), vec!["dbo".into(), table.clone()]),
        _ => (table.clone(), vec![table.clone()]),
    };
    let manager = ConnectionPoolManager::new(None);
    manager
        .create_connection(config)
        .await
        .map_err(|_| "连接指定测试数据库失败".to_string())?;
    let mut created = false;
    let outcome = async {
        execute(&manager, &id, &database,
            &format!("CREATE TABLE {qualified} ({KEY_COLUMN} BIGINT PRIMARY KEY, plain_value VARCHAR(80) NOT NULL)"), "建测试表").await?;
        created = true;
        execute(&manager, &id, &database,
            &format!("INSERT INTO {qualified} ({KEY_COLUMN}, plain_value) VALUES ({EXACT_KEY}, 'context fixture')"), "插入测试行").await?;
        check_contexts(&manager, &id, &database, &table, &parts).await
    }.await;
    // 所有校验均在异步 Result 中执行，插入、读取或校验失败也会走到这里。
    let cleanup = if created {
        execute(
            &manager,
            &id,
            &database,
            &format!("DROP TABLE {qualified}"),
            "清理本次唯一测试表",
        )
        .await
    } else {
        Ok(())
    };
    let disconnect = manager
        .disconnect(&id)
        .await
        .map_err(|_| "断开测试连接失败".to_string());
    let failures = [outcome, cleanup, disconnect]
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("；"))
    }
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 指定专用测试库"]
async fn mysql_controlled_result_context() {
    exercise("mysql", "SQLINK_TEST_MYSQL")
        .await
        .expect("MySQL 真实受控结果集成测试失败");
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 指定专用测试库"]
async fn postgres_controlled_result_context() {
    exercise("postgres", "SQLINK_TEST_POSTGRES")
        .await
        .expect("PostgreSQL 真实受控结果集成测试失败");
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MSSQL 指定专用测试库"]
async fn mssql_controlled_result_context() {
    exercise("mssql", "SQLINK_TEST_MSSQL")
        .await
        .expect("SQL Server 真实受控结果集成测试失败");
}
