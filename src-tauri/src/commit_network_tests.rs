//! 真实网络提交闭环：只使用 SQLINK_TEST_* 显式专用测试库，默认忽略。
//! 所有对象位于随机 schema/database；断言失败前先清理自有对象，不读取应用凭据。

use super::commit_internal;
use crate::changeset::{placeholder, CellInput, ChangeOperation, ChangeSetRequest};
use crate::commit_receipt::{CommitReceipt, CommitStatus, OperationOutcome};
use crate::database::{create_and_connect_driver, pool::ConnectionPoolManager, DatabaseDriver};
use crate::models::ConnectionConfig;
use crate::table_result::{read_table_page, TablePage, TablePageRequest};
use crate::task_registry::{CleanupState, CommitState};
use crate::typed_value::TypedValue;
use reqwest::Url;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, String>;

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

fn config_from_env(kind: &str, variable: &str, id: &str) -> TestResult<ConnectionConfig> {
    let raw =
        std::env::var(variable).map_err(|_| format!("必须设置 {variable} 才能显式运行此测试"))?;
    // 不输出原始连接串和解析错误，避免凭据出现在测试日志。
    let url = Url::parse(&raw).map_err(|_| format!("{variable} 不是有效测试连接串"))?;
    require(
        match kind {
            "postgres" => matches!(url.scheme(), "postgres" | "postgresql"),
            "mssql" => matches!(url.scheme(), "mssql" | "sqlserver"),
            _ => url.scheme() == kind,
        },
        "测试连接串协议与驱动不符",
    )?;
    require(url.fragment().is_none(), "测试连接串不支持片段")?;
    let database = decode_component(url.path().trim_start_matches('/'))?;
    require(!database.is_empty(), "测试连接串必须显式指定数据库")?;
    let mut encrypt = None;
    for (name, value) in url.query_pairs() {
        match (kind, name.as_ref(), value.as_ref()) {
            ("mssql", "encrypt", "true") => encrypt = Some(true),
            ("mssql", "encrypt", "false") => encrypt = Some(false),
            _ => return Err("测试连接串包含不支持的参数".into()),
        }
    }
    Ok(ConnectionConfig {
        id: Some(id.into()),
        name: format!("受控提交网络回归 {kind}"),
        driver_type: kind.into(),
        host: url
            .host_str()
            .ok_or("测试连接串缺少主机")?
            .trim_start_matches('[')
            .trim_end_matches(']')
            .into(),
        port: url.port().unwrap_or(match kind {
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

fn quoted(kind: &str, name: &str) -> String {
    if kind == "mysql" {
        format!("`{name}`")
    } else {
        format!("\"{name}\"")
    }
}

struct Fixture {
    kind: String,
    manager: ConnectionPoolManager,
    external: Arc<dyn DatabaseDriver>,
    connection_id: String,
    base_database: String,
    namespace: String,
    database: String,
    target: String,
    definition: String,
    config: ConnectionConfig,
}

async fn mysql_fixture_account(fixture: &Fixture) -> TestResult<(String, String)> {
    let account = fixture
        .external
        .query_typed("SELECT CURRENT_USER()", &[], Some(&fixture.base_database))
        .await
        .map_err(|_| "无法核对显式测试账号")?;
    let account = match account.typed_rows.first().and_then(|row| row.first()) {
        Some(Some(TypedValue::Text(account))) => account,
        _ => return Err("测试账号身份不是可信文本".into()),
    };
    let (user, host) = account.rsplit_once('@').ok_or("测试账号身份格式无效")?;
    require(
        !user.is_empty()
            && !host.is_empty()
            && user.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._:%-".contains(&c)),
        "测试账号字符范围未经验证，不能构造授权",
    )?;
    Ok((user.into(), host.into()))
}

async fn grant_mysql_fixture_trigger(fixture: &Fixture) -> TestResult<String> {
    let (user, host) = mysql_fixture_account(fixture).await?;
    let grantee = format!("'{user}'@'{host}'");
    // 只向显式隔离测试账号授本次随机 schema 的权限；结束时撤销，不改全局权限。
    fixture
        .external
        .execute(
            &format!("GRANT TRIGGER ON `{}`.* TO {grantee}", fixture.namespace),
            Some(&fixture.base_database),
            None,
        )
        .await
        .map_err(|_| "无法授予自有测试schema的直接TRIGGER权限")?;
    Ok(grantee)
}

impl Fixture {
    async fn read(&self) -> TestResult<TablePage> {
        read_table_page(
            &self.manager,
            TablePageRequest {
                protocol_version: 1,
                connection_id: self.connection_id.clone(),
                database: self.database.clone(),
                table_parts: vec![self.namespace.clone(), "commit_rows".into()],
                page: 1,
                page_size: 20,
                sort: None,
            },
        )
        .await
        .map_err(|error| format!("受控读取失败：{}", error.code))
    }

    async fn stored(&self) -> TestResult<Vec<Vec<Option<TypedValue>>>> {
        self.external
            .query_typed(
                &format!(
                    "SELECT id,payload,nullable_text,bytes,big_number FROM {} ORDER BY id",
                    self.target
                ),
                &[],
                Some(&self.database),
            )
            .await
            .map(|result| result.typed_rows)
            .map_err(|_| "独立连接读回失败".into())
    }

    async fn seed(&self) -> TestResult {
        let params = (1..=5)
            .map(|i| placeholder(&self.kind, i))
            .collect::<Vec<_>>()
            .join(",");
        for (id, payload) in [("1", "第一行"), ("2", "第二行")] {
            self.external.execute_bound(&format!("INSERT INTO {}(id,payload,nullable_text,bytes,big_number) VALUES({params})", self.target), &[
                TypedValue::Integer(id.into()), TypedValue::Text(payload.into()), TypedValue::Text("初值".into()),
                TypedValue::binary(&[0, 255, 0]), TypedValue::Integer("0".into()),
            ], Some(&self.database)).await.map_err(|_| "初始化自有测试行失败")?;
        }
        Ok(())
    }

    async fn commit(&self, request: ChangeSetRequest) -> TestResult<CommitReceipt> {
        commit_internal(&self.manager, request, None, true)
            .await
            .map_err(|error| format!("提交协调器拒绝：{}", error.code))
    }
}

fn changes(
    page: &TablePage,
    values: &[(&str, TypedValue)],
) -> TestResult<BTreeMap<String, CellInput>> {
    values
        .iter()
        .map(|(name, value)| {
            let column = page
                .context
                .columns
                .iter()
                .find(|column| column.display_name == *name)
                .ok_or("缺少预期受控列")?;
            Ok((
                column.column_id.clone(),
                CellInput::Set {
                    value: value.clone(),
                },
            ))
        })
        .collect()
}

fn handle(page: &TablePage, id: &str) -> TestResult<String> {
    let index = page
        .typed_rows
        .iter()
        .position(|row| row.first() == Some(&Some(TypedValue::Integer(id.into()))))
        .ok_or("缺少预期测试行")?;
    page.row_handles
        .get(index)
        .and_then(Clone::clone)
        .ok_or_else(|| "真实网络读取未授予可信行句柄".into())
}

fn update(
    page: &TablePage,
    id: &str,
    values: &[(&str, TypedValue)],
) -> TestResult<ChangeOperation> {
    Ok(ChangeOperation::Update {
        op_id: Uuid::new_v4().to_string(),
        row_handle: handle(page, id)?,
        changes: changes(page, values)?,
    })
}

fn request(page: &TablePage, operations: Vec<ChangeOperation>) -> ChangeSetRequest {
    ChangeSetRequest {
        protocol_version: 1,
        change_set_id: Uuid::new_v4().to_string(),
        result_id: page.context.result_id.clone(),
        revision: page.context.revision,
        operations,
    }
}

fn committed(receipt: &CommitReceipt) -> TestResult {
    require(
        receipt.status == CommitStatus::Committed
            && receipt.commit_state == CommitState::Committed
            && receipt.cleanup_state == CleanupState::Succeeded
            && receipt
                .operation_results
                .iter()
                .all(|op| op.outcome == OperationOutcome::Applied && op.affected_rows == Some(1)),
        &format!("真实提交必须确认成功，不能把只读拒绝算作通过：{receipt:?}"),
    )
}

async fn crud_exact_values_and_receipt_idempotence(fixture: &Fixture) -> TestResult {
    let page = fixture.read().await?;
    let request = request(
        &page,
        vec![
            update(
                &page,
                "1",
                &[
                    ("id", TypedValue::Integer("3".into())),
                    ("payload", TypedValue::Text("NULL".into())),
                    ("nullable_text", TypedValue::Null),
                    ("bytes", TypedValue::binary(&[0, 255, 128, 0])),
                    ("big_number", TypedValue::Integer(i64::MAX.to_string())),
                ],
            )?,
            ChangeOperation::Insert {
                op_id: Uuid::new_v4().to_string(),
                changes: changes(
                    &page,
                    &[
                        ("id", TypedValue::Integer("9007199254740993".into())),
                        ("payload", TypedValue::Text("新增行".into())),
                        ("nullable_text", TypedValue::Text("".into())),
                        ("bytes", TypedValue::binary(&[])),
                        ("big_number", TypedValue::Integer(i64::MIN.to_string())),
                    ],
                )?,
            },
            ChangeOperation::Delete {
                op_id: Uuid::new_v4().to_string(),
                row_handle: handle(&page, "2")?,
            },
        ],
    );
    let receipt = fixture.commit(request.clone()).await?;
    committed(&receipt)?;
    let expected = vec![
        vec![
            Some(TypedValue::Integer("3".into())),
            Some(TypedValue::Text("NULL".into())),
            Some(TypedValue::Null),
            Some(TypedValue::binary(&[0, 255, 128, 0])),
            Some(TypedValue::Integer(i64::MAX.to_string())),
        ],
        vec![
            Some(TypedValue::Integer("9007199254740993".into())),
            Some(TypedValue::Text("新增行".into())),
            Some(TypedValue::Text("".into())),
            Some(TypedValue::binary(&[])),
            Some(TypedValue::Integer(i64::MIN.to_string())),
        ],
    ];
    require(
        fixture.stored().await? == expected,
        "独立连接必须精确读回增删改、大整数主键、i64边界、NULL/空文本/二进制",
    )?;
    let repeated = fixture.commit(request.clone()).await?;
    require(
        serde_json::to_value(&receipt).map_err(|_| "回执编码失败")?
            == serde_json::to_value(&repeated).map_err(|_| "回执编码失败")?,
        "同变更ID必须返回完全相同回执",
    )?;
    let queried = fixture
        .manager
        .commit_receipts
        .get(&request.change_set_id, &request.result_id, request.revision)
        .map_err(|error| error.code)?;
    require(
        serde_json::to_value(&receipt).map_err(|_| "回执编码失败")?
            == serde_json::to_value(&queried).map_err(|_| "回执编码失败")?,
        "独立查询回执必须与提交回执一致",
    )?;
    require(
        fixture.stored().await? == expected,
        "重复请求不能重放插入或改变数据库",
    )?;
    require(
        fixture.manager.tasks.active_count() == 0,
        "提交后不能残留活动任务",
    )
}

async fn conflict_and_constraint_failure_are_atomic(fixture: &Fixture) -> TestResult {
    let page = fixture.read().await?;
    fixture
        .external
        .execute_bound(
            &format!(
                "UPDATE {} SET payload={} WHERE id={}",
                fixture.target,
                placeholder(&fixture.kind, 1),
                placeholder(&fixture.kind, 2)
            ),
            &[
                TypedValue::Text("外部修改".into()),
                TypedValue::Integer("2".into()),
            ],
            Some(&fixture.database),
        )
        .await
        .map_err(|_| "外部连接修改失败")?;
    let before = fixture.stored().await?;
    let receipt = fixture
        .commit(request(
            &page,
            vec![
                update(
                    &page,
                    "1",
                    &[("payload", TypedValue::Text("组内第一行".into()))],
                )?,
                update(
                    &page,
                    "2",
                    &[("payload", TypedValue::Text("组内第二行".into()))],
                )?,
            ],
        ))
        .await?;
    require(
        receipt.status == CommitStatus::Conflict
            && receipt.error_code.as_deref() == Some("ROW_CHANGED")
            && receipt.operation_results[0].outcome == OperationOutcome::NotExecuted
            && receipt.operation_results[1].outcome == OperationOutcome::Conflict,
        &format!("外部改行必须在任何DML之前阻断整组：{receipt:?}"),
    )?;
    require(
        fixture.stored().await? == before,
        "冲突必须保留外部修改，组内第一行也不得保存",
    )?;
    let page = fixture.read().await?;
    let receipt = fixture
        .commit(request(
            &page,
            vec![
                update(
                    &page,
                    "1",
                    &[("payload", TypedValue::Text("组内唯一值".into()))],
                )?,
                update(
                    &page,
                    "2",
                    &[("payload", TypedValue::Text("组内唯一值".into()))],
                )?,
            ],
        ))
        .await?;
    require(
        receipt.status == CommitStatus::RolledBack
            && receipt.commit_state == CommitState::RolledBack
            && receipt.rollback_state == CleanupState::Succeeded
            && receipt.cleanup_state == CleanupState::Succeeded
            && receipt.error_code.as_deref() == Some("OPERATION_FAILED")
            && receipt
                .operation_results
                .iter()
                .all(|op| op.outcome == OperationOutcome::RolledBack),
        &format!("第二项约束失败必须确认整组回滚：{receipt:?}"),
    )?;
    require(
        fixture.stored().await? == before,
        "后项约束失败后第一项已执行DML也必须回滚",
    )?;
    require(
        fixture.manager.tasks.active_count() == 0,
        "冲突和回滚后不能残留活动任务",
    )
}

async fn recreated_object_rejects_old_context(fixture: &Fixture) -> TestResult {
    let page = fixture.read().await?;
    fixture
        .external
        .execute(
            &format!("DROP TABLE {}", fixture.target),
            Some(&fixture.database),
            None,
        )
        .await
        .map_err(|_| "删除自有对象失败")?;
    fixture
        .external
        .execute(&fixture.definition, Some(&fixture.database), None)
        .await
        .map_err(|_| "同名同结构重建失败")?;
    fixture.seed().await?;
    let before = fixture.stored().await?;
    let receipt = fixture
        .commit(request(
            &page,
            vec![update(
                &page,
                "1",
                &[("payload", TypedValue::Text("旧快照不得写入".into()))],
            )?],
        ))
        .await?;
    require(
        receipt.status == CommitStatus::FailedBeforeCommit
            && receipt.error_code.as_deref() == Some("OBJECT_IDENTITY_CHANGED")
            && receipt
                .operation_results
                .iter()
                .all(|op| op.outcome == OperationOutcome::NotExecuted),
        &format!("同名同结构且同原值的新对象必须拒绝旧快照：{receipt:?}"),
    )?;
    require(fixture.stored().await? == before, "旧快照不能修改新建对象")
}

async fn held_global_grant_cannot_prove_trigger_visibility(fixture: &Fixture) -> TestResult {
    let user = format!("priv_{}", &Uuid::new_v4().simple().to_string()[..20]);
    let password = Uuid::new_v4().simple().to_string();
    // 端口映射会改变服务端看到的客户端地址；沿用已认证账号的授权host，不能从URL推断。
    let (_, host) = mysql_fixture_account(fixture).await?;
    let grantee = format!("'{user}'@'{host}'");
    fixture.external.execute(&format!("CREATE TRIGGER `{}`.held_visibility BEFORE INSERT ON {} FOR EACH ROW SET NEW.payload='触发器实际执行'", fixture.namespace, fixture.target), Some(&fixture.database), None)
        .await.map_err(|_| "创建自有可见性测试触发器失败")?;
    fixture
        .external
        .execute(
            &format!("CREATE USER {grantee} IDENTIFIED BY '{password}'"),
            Some(&fixture.base_database),
            None,
        )
        .await
        .map_err(|_| "创建随机权限测试账号失败")?;
    let mut connected = None;
    let result: TestResult = async {
        fixture.external.execute(&format!("GRANT SELECT ON `{}`.* TO {grantee}", fixture.namespace), Some(&fixture.base_database), None)
            .await.map_err(|_| "授予随机账号测试库SELECT失败")?;
        fixture.external.execute(&format!("GRANT PROCESS ON *.* TO {grantee}"), Some(&fixture.base_database), None)
            .await.map_err(|_| "授予随机账号目录读取权限失败")?;
        let mut config = fixture.config.clone();
        config.user = user.clone();
        config.password = password;
        config.database = Some(fixture.database.clone());
        let driver = create_and_connect_driver(&config).await.map_err(|_| "随机权限测试账号连接失败")?;
        connected = Some(driver.clone());
        let target = driver.resolve_table(&fixture.database, &[fixture.namespace.clone(), "commit_rows".into()]).await.map_err(|_| "随机账号目标解析失败")?;
        let mut lease = driver.lease_session(Some(&fixture.database)).await.map_err(|_| "持有会话获取失败")?;
        let test: TestResult = async {
            lease.begin_write().await.map_err(|_| "持有会话事务启动失败")?;
            let sql = "SELECT (SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE='commit_rows'), (SELECT COUNT(*) FROM information_schema.USER_PRIVILEGES WHERE GRANTEE=? AND PRIVILEGE_TYPE='TRIGGER')";
            let params = [TypedValue::Text(fixture.namespace.clone()), TypedValue::Text(grantee.clone())];
            let counts = |read: crate::typed_value::TypedQueryResult| -> TestResult<Vec<u64>> {
                read.typed_rows.first().ok_or("权限计数缺少结果")?.iter().map(|value| match value {
                    Some(TypedValue::Integer(value) | TypedValue::UnsignedInteger(value)) => value.parse::<u64>().map_err(|_| "权限计数不是非负整数".into()),
                    _ => Err("权限计数类型无效".into()),
                }).collect()
            };
            let before = counts(lease.query_typed(sql, &params).await.map_err(|_| "读取授权前权限证据失败")?)?;
            require(before == vec![0, 0], "无TRIGGER会话应隐藏真实触发器，且没有全局TRIGGER记录")?;
            fixture.external.execute(&format!("GRANT TRIGGER ON *.* TO {grantee}"), Some(&fixture.base_database), None)
                .await.map_err(|_| "隔离账号全局授权失败")?;
            let after = counts(lease.query_typed(sql, &params).await.map_err(|_| "读取持有会话授权后证据失败")?)?;
            require(after == vec![0, 1], "实库反例必须证实：目录已有全局授权，但当前物理会话仍看不见触发器")?;
            let inspected = lease.inspect_target(&target, true).await.map_err(|_| "持有会话对象检查失败")?;
            require(inspected.write_reasons.iter().any(|code| code == "TRIGGER_VISIBILITY_UNVERIFIED" || code == "TRIGGERS_UNSUPPORTED"),
                "目录全局授权不能替代本租约有效权限，不得放行隐藏的真实触发器")
        }.await;
        let rollback = lease.rollback().await;
        let release = lease.release().await;
        test?;
        require(rollback.is_ok() && release.is_ok(), "持有权限测试租约清理失败")
    }.await;
    let disconnected = match connected {
        Some(driver) => driver.disconnect().await.is_ok(),
        None => true,
    };
    let dropped = fixture
        .external
        .execute(
            &format!("DROP USER {grantee}"),
            Some(&fixture.base_database),
            None,
        )
        .await
        .is_ok();
    match result {
        Err(error) if !disconnected || !dropped => {
            Err(format!("{error}；随机权限测试账号清理也失败"))
        }
        Err(error) => Err(error),
        Ok(()) => require(disconnected && dropped, "随机权限测试账号或连接清理失败"),
    }
}

async fn disabled_session_checks_block_writes_and_do_not_leak_to_next_lease(
    fixture: &Fixture,
) -> TestResult {
    let driver = fixture
        .manager
        .get_connection(&fixture.connection_id)
        .await
        .map_err(|_| "获取约束门禁测试连接失败")?;
    let target = driver
        .resolve_table(
            &fixture.database,
            &[fixture.namespace.clone(), "commit_rows".into()],
        )
        .await
        .map_err(|_| "解析约束门禁目标失败")?;
    let before = fixture.stored().await?;
    let checks_sql = "SELECT @@SESSION.foreign_key_checks, @@SESSION.unique_checks";
    let enabled = |read: &crate::typed_value::TypedQueryResult| {
        read.typed_rows.len() == 1 && read.typed_rows[0].len() == 2
            && read.typed_rows[0].iter().all(|value| matches!(value, Some(TypedValue::Integer(value) | TypedValue::UnsignedInteger(value)) if value == "1"))
    };
    let mut lease = driver
        .lease_session(Some(&fixture.database))
        .await
        .map_err(|_| "获取约束门禁租约失败")?;
    let test: TestResult = async {
        let baseline = lease
            .query_typed(checks_sql, &[])
            .await
            .map_err(|_| "读取基线约束状态失败")?;
        require(enabled(&baseline), "隔离测试实例基线必须启用外键与唯一检查")?;
        // 只改本物理会话；不改服务器全局值，也不执行任何DML。
        lease
            .execute("SET SESSION foreign_key_checks=0, unique_checks=0")
            .await
            .map_err(|_| "设置本租约约束状态失败")?;
        lease
            .begin_write()
            .await
            .map_err(|_| "启动约束门禁测试事务失败")?;
        let inspected = lease
            .inspect_target(&target, true)
            .await
            .map_err(|_| "同租约约束门禁检查失败")?;
        require(
            inspected
                .write_reasons
                .iter()
                .any(|reason| reason == "FOREIGN_KEY_CHECKS_DISABLED")
                && inspected
                    .write_reasons
                    .iter()
                    .any(|reason| reason == "UNIQUE_CHECKS_DISABLED"),
            "本租约关闭外键和唯一检查后必须同时拒绝写入能力",
        )
    }
    .await;
    let rollback = lease.rollback().await;
    let release = lease.release().await;
    test?;
    require(
        rollback.is_ok() && release.is_ok(),
        "约束门禁测试必须明确回滚并归还租约",
    )?;
    // 连续获取归还，覆盖原物理连接被池复用以及回收稍迟的情况；不能遗留0/0。
    for _ in 0..2 {
        let mut next = driver
            .lease_session(Some(&fixture.database))
            .await
            .map_err(|_| "获取后续租约失败")?;
        let state = next.query_typed(checks_sql, &[]).await;
        let released = next.release().await;
        let state = state.map_err(|_| "读取后续租约约束状态失败")?;
        require(
            enabled(&state),
            "新租约必须恢复1/1，不能继承关闭的外键或唯一检查",
        )?;
        require(released.is_ok(), "后续租约归还失败")?;
    }
    require(
        fixture.stored().await? == before,
        "约束门禁测试不得更改任何测试行",
    )
}

#[derive(Clone, Copy)]
enum Scenario {
    Crud,
    Atomicity,
    Recreated,
    HeldPrivilege,
    SessionChecks,
}

async fn run(kind: &str, variable: &str, scenario: Scenario) -> TestResult {
    let nonce = Uuid::new_v4().simple().to_string();
    let connection_id = format!("commit_{nonce}");
    let config = config_from_env(kind, variable, &connection_id)?;
    let base_database = config.database.clone().ok_or("缺少测试库")?;
    let external = create_and_connect_driver(&config)
        .await
        .map_err(|_| "无法连接显式测试库")?;
    let manager = ConnectionPoolManager::new(None);
    if manager.create_connection(config.clone()).await.is_err() {
        let _ = external.disconnect().await;
        return Err("受控连接创建失败".into());
    }
    let namespace = format!("commit_{nonce}");
    let database = if kind == "mysql" {
        namespace.clone()
    } else {
        base_database.clone()
    };
    let target = format!(
        "{}.{}",
        quoted(kind, &namespace),
        quoted(kind, "commit_rows")
    );
    let definition = format!("CREATE TABLE {target}(id BIGINT NOT NULL PRIMARY KEY, payload VARCHAR(64) NOT NULL UNIQUE, nullable_text VARCHAR(64) NULL, bytes {} NULL, big_number BIGINT NULL){}",
        if kind == "mysql" { "VARBINARY(32)" } else { "BYTEA" }, if kind == "mysql" { " ENGINE=InnoDB DEFAULT CHARSET=utf8mb4" } else { "" });
    let fixture = Fixture {
        kind: kind.into(),
        manager,
        external,
        connection_id,
        base_database,
        namespace,
        database,
        target,
        definition,
        config,
    };
    let mut namespace_created = false;
    let mut trigger_grantee = None;
    let result: TestResult = async {
        fixture
            .external
            .execute(
                &format!(
                    "CREATE {} {}",
                    if kind == "mysql" {
                        "DATABASE"
                    } else {
                        "SCHEMA"
                    },
                    quoted(kind, &fixture.namespace)
                ),
                Some(&fixture.base_database),
                None,
            )
            .await
            .map_err(|_| "无法创建随机测试namespace")?;
        namespace_created = true;
        if kind == "mysql" {
            trigger_grantee = Some(grant_mysql_fixture_trigger(&fixture).await?);
        }
        fixture
            .external
            .execute(&fixture.definition, Some(&fixture.database), None)
            .await
            .map_err(|_| "无法创建随机自有表")?;
        fixture.seed().await?;
        match scenario {
            Scenario::Crud => crud_exact_values_and_receipt_idempotence(&fixture).await,
            Scenario::Atomicity => conflict_and_constraint_failure_are_atomic(&fixture).await,
            Scenario::Recreated => recreated_object_rejects_old_context(&fixture).await,
            Scenario::HeldPrivilege => {
                held_global_grant_cannot_prove_trigger_visibility(&fixture).await
            }
            Scenario::SessionChecks => {
                disabled_session_checks_block_writes_and_do_not_leak_to_next_lease(&fixture).await
            }
        }
    }
    .await;
    // 无论场景是否通过，先清理本次创建的对象；清理错误也必须报告失败。
    let mut cleanup_ok = true;
    if let Some(grantee) = trigger_grantee {
        cleanup_ok &= fixture
            .external
            .execute(
                &format!("REVOKE TRIGGER ON `{}`.* FROM {grantee}", fixture.namespace),
                Some(&fixture.base_database),
                None,
            )
            .await
            .is_ok();
    }
    if namespace_created {
        cleanup_ok &= fixture
            .external
            .execute(
                &format!("DROP TABLE IF EXISTS {}", fixture.target),
                Some(&fixture.base_database),
                None,
            )
            .await
            .is_ok();
        cleanup_ok &= fixture
            .external
            .execute(
                &format!(
                    "DROP {} {}",
                    if kind == "mysql" {
                        "DATABASE"
                    } else {
                        "SCHEMA"
                    },
                    quoted(kind, &fixture.namespace)
                ),
                Some(&fixture.base_database),
                None,
            )
            .await
            .is_ok();
    }
    cleanup_ok &= fixture
        .manager
        .disconnect(&fixture.connection_id)
        .await
        .is_ok();
    cleanup_ok &= fixture.external.disconnect().await.is_ok();
    match result {
        Err(error) if !cleanup_ok => Err(format!("{error}；测试自有对象或连接清理也失败")),
        Err(error) => Err(error),
        Ok(()) => require(cleanup_ok, "测试自有对象或连接清理失败"),
    }
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 显式专用测试库"]
async fn postgres_crud_exact_values_and_receipt_idempotence() {
    run("postgres", "SQLINK_TEST_POSTGRES", Scenario::Crud)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 显式专用测试库"]
async fn postgres_conflict_and_constraint_failure_are_atomic() {
    run("postgres", "SQLINK_TEST_POSTGRES", Scenario::Atomicity)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_POSTGRES 显式专用测试库"]
async fn postgres_recreated_object_rejects_old_context() {
    run("postgres", "SQLINK_TEST_POSTGRES", Scenario::Recreated)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 显式专用8.x测试库与完整目录权限"]
async fn mysql_crud_exact_values_and_receipt_idempotence() {
    run("mysql", "SQLINK_TEST_MYSQL", Scenario::Crud)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 显式专用8.x测试库与完整目录权限"]
async fn mysql_conflict_and_constraint_failure_are_atomic() {
    run("mysql", "SQLINK_TEST_MYSQL", Scenario::Atomicity)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 显式专用8.x测试库与完整目录权限"]
async fn mysql_recreated_object_rejects_old_context() {
    run("mysql", "SQLINK_TEST_MYSQL", Scenario::Recreated)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 显式专用8.4测试库及创建临时账号权限"]
async fn mysql_held_global_grant_cannot_prove_trigger_visibility() {
    run("mysql", "SQLINK_TEST_MYSQL", Scenario::HeldPrivilege)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "需要 SQLINK_TEST_MYSQL 显式专用8.4测试库与直接schema目录权限"]
async fn mysql_disabled_session_checks_block_writes_and_do_not_leak_to_next_lease() {
    run("mysql", "SQLINK_TEST_MYSQL", Scenario::SessionChecks)
        .await
        .unwrap();
}
