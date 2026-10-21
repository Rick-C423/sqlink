//! 显式运行需 SQLINK_TEST_POSTGRES / SQLINK_TEST_MYSQL；缺少测试库必须硬失败。
//! 仅创建/删除本次随机夹具表，错误日志不输出连接串。
use reqwest::Url;
use serde_json::json;
use sqlconnect_lib::database::pool::ConnectionPoolManager;
use sqlconnect_lib::dataset::{self, DatasetPageRequest, TableCountRequest};
use sqlconnect_lib::models::ConnectionConfig;
use sqlconnect_lib::typed_value::TypedValue;
use uuid::Uuid;

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

async fn run_dataset(driver_kind: &str, env: &str) -> TestResult {
    let id = Uuid::new_v4().to_string();
    let config = config_from_env(driver_kind, env, &id)?;
    let database = config.database.clone().ok_or("测试数据库缺失")?;
    let manager = ConnectionPoolManager::new(None);
    manager
        .create_connection(config)
        .await
        .map_err(|_| "测试连接失败")?;
    let driver = manager
        .get_connection(&id)
        .await
        .map_err(|_| "测试连接不可用")?;
    let name = format!("sqlink_ds_{}", Uuid::new_v4().simple());
    let (target, parts) = if driver_kind == "postgres" {
        (
            format!("\"public\".\"{name}\""),
            vec!["public".to_string(), name.clone()],
        )
    } else {
        (format!("`{name}`"), vec![database.clone(), name.clone()])
    };
    let create = format!("CREATE TABLE {target} (record_key BIGINT NOT NULL PRIMARY KEY, rank_value INTEGER NOT NULL, label VARCHAR(32))");
    driver
        .execute(&create, Some(&database), None)
        .await
        .map_err(|_| "创建测试表失败")?;
    let result: TestResult = async {
        driver.execute(&format!("INSERT INTO {target} VALUES (9007199254740993,2,'one'),(9007199254740994,2,'two'),(9007199254740995,1,'three')"), Some(&database), None).await.map_err(|_| "写入夹具失败")?;
        let request = |page: u32, cursor: Option<String>| -> DatasetPageRequest { serde_json::from_value(json!({"request":{"protocolVersion":1,"connectionId":id,"database":database,"tableParts":parts,"page":page,"pageSize":1,"sort":{"column":"rank_value","direction":"desc"}},"cursor":cursor,"taskId":Uuid::new_v4().to_string()})).expect("固定请求夹具有效") };
        let first = dataset::read_page(&manager, request(1,None)).await.map_err(|value| format!("首屏失败：{}",value.code))?;
        require(first.page.total_rows.is_none(), "首屏不应含同步 COUNT")?;
        require(first.has_more && first.page.row_handles.len()==1 && first.page.typed_rows.len()==1, "额外探测行不应进入结果和句柄")?;
        require(first.next_cursor.is_some(), "安全整数排序应生成 opaque 游标")?;
        require(first.page.typed_rows[0][0] == Some(TypedValue::Integer("9007199254740993".into())), "首行大整数必须无损")?;
        let total = dataset::count_table(&manager, TableCountRequest { protocol_version:1, result_id:first.page.context.result_id.clone(), revision:1, task_id:Some(Uuid::new_v4().to_string()) }).await.map_err(|value| format!("独立统计失败：{}",value.code))?;
        require(total.total_rows=="3", "COUNT 应返回完整十进制文本")?;
        driver.execute(&format!("INSERT INTO {target} VALUES (9007199254740992,3,'inserted')"), Some(&database), None).await.map_err(|_| "前置插入失败")?;
        let mut changed = request(2,first.next_cursor.clone()); changed.request.page_size=2;
        require(dataset::read_page(&manager, changed).await.err().is_some_and(|value| value.code=="CURSOR_SOURCE_MISMATCH"), "变更分页条件必须拒绝旧游标")?;
        let second = dataset::read_page(&manager, request(2,first.next_cursor)).await.map_err(|value| format!("keyset 次页失败：{}",value.code))?;
        require(second.pagination_mode=="keyset", "连续次页必须走 keyset")?;
        require(second.page.typed_rows[0][0]==Some(TypedValue::Integer("9007199254740994".into())), "前插后次页不能重复首行")?;
        let third = dataset::read_page(&manager, request(3,second.next_cursor)).await.map_err(|value| format!("keyset 尾页失败：{}",value.code))?;
        require(third.page.typed_rows[0][0]==Some(TypedValue::Integer("9007199254740995".into())) && !third.has_more, "混合升降序应定位正确尾行")?;
        require(manager.tasks.active_count()==0, "完成后不能遗留活动任务")?;
        Ok(())
    }.await;
    let cleanup = driver
        .execute(&format!("DROP TABLE {target}"), Some(&database), None)
        .await
        .map_err(|_| "夹具清理失败");
    let disconnect = manager
        .disconnect(&id)
        .await
        .map_err(|_| "测试连接清理失败");
    result?;
    cleanup?;
    disconnect?;
    Ok(())
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES，显式运行缺失配置必须失败"]
async fn dataset_postgres_real_keyset_and_count() {
    run_dataset("postgres", "SQLINK_TEST_POSTGRES")
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL，显式运行缺失配置必须失败"]
async fn dataset_mysql_real_keyset_and_count() {
    run_dataset("mysql", "SQLINK_TEST_MYSQL").await.unwrap();
}
