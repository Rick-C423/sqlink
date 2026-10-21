//! 显式运行的真实 SQLite 百万行验收；只核对功能，不设置机器性能门槛。
//! 进程内不实现平台内存采样，外部采样器通过报告中的 process_id 关联进程。

use serde_json::{json, Value};
use sqlconnect_lib::{
    database::pool::ConnectionPoolManager,
    dataset::{self, DatasetPage, DatasetPageRequest, TableCountRequest},
    models::ConnectionConfig,
    table_result::{self, ContextReference},
    typed_value::TypedValue,
};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use uuid::Uuid;

const ROW_COUNT: u32 = 1_000_000;
const PAGE_SIZE: u32 = 100;
const CONNECTION_ID: &str = "dataset_performance";

struct TemporaryDatabase(PathBuf);

impl TemporaryDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("sqlink-p2-performance-{}", Uuid::new_v4().simple()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn path(&self) -> PathBuf {
        self.0.join("million.sqlite")
    }
}

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        // 路径只来自本测试刚创建的随机 Temp 子目录，不接受外部删除路径。
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn output_path() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    let requested = std::env::var_os("SQLINK_PERF_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join(".zcode/p2-closeout-performance.json"));
    let path = if requested.is_absolute() {
        requested
    } else {
        workspace.join(requested)
    };
    // 两侧父目录都规范化，兼容 Windows 普通绝对路径和 canonicalize 扩展前缀。
    // 报告仅允许写入已有 .zcode 目录树，不为外部输入创建目录。
    let allowed = workspace.join(".zcode").canonicalize().unwrap();
    let parent = path.parent().unwrap();
    let normalized_parent = parent.canonicalize().unwrap();
    assert!(
        normalized_parent.starts_with(&allowed),
        "报告目录必须位于本工作区 .zcode 内"
    );
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        assert!(!metadata.file_type().is_symlink(), "报告文件不能是符号链接");
    }
    normalized_parent.join(path.file_name().unwrap())
}

fn write_report(path: &Path, report: &Value) {
    std::fs::write(path, serde_json::to_vec_pretty(report).unwrap()).unwrap();
}

fn request(page: u32, cursor: Option<String>) -> DatasetPageRequest {
    serde_json::from_value(json!({
        "request": {
            "protocolVersion": 1, "connectionId": CONNECTION_ID,
            "database": "main", "tableParts": ["main", "million_rows"],
            "page": page, "pageSize": PAGE_SIZE,
            "sort": {"column": "id", "direction": "asc"}
        },
        "taskId": Uuid::new_v4().to_string(), "cursor": cursor
    }))
    .unwrap()
}

fn check_page(result: &DatasetPage, page: u32) {
    let start = (page - 1) * PAGE_SIZE + 1;
    let expected_rows = (ROW_COUNT - start + 1).min(PAGE_SIZE) as usize;
    assert_eq!(result.page.result.rows.len(), expected_rows);
    assert_eq!(result.page.typed_rows.len(), expected_rows);
    assert_eq!(result.has_more, page * PAGE_SIZE < ROW_COUNT);
    assert!(!result.page.result.truncated);
    assert!(result.page.total_rows.is_none(), "取页不能同步返回 COUNT");
    for (index, row) in result.page.typed_rows.iter().enumerate() {
        assert_eq!(
            row.first().and_then(Option::as_ref),
            Some(&TypedValue::Integer((start + index as u32).to_string()))
        );
        match row.get(1).and_then(Option::as_ref) {
            Some(TypedValue::Text(payload)) => {
                assert_eq!(payload.as_bytes(), &[b'x'; 128]);
            }
            _ => panic!("分页 payload 必须保留原始文本"),
        }
    }
    if !result.has_more {
        assert!(result.next_cursor.is_none());
    }
}

fn retire(manager: &ConnectionPoolManager, page: &DatasetPage) {
    table_result::release_context(
        manager,
        ContextReference {
            protocol_version: 1,
            result_id: page.page.context.result_id.clone(),
            revision: page.page.context.revision,
        },
    )
    .unwrap();
}

async fn measure_page(
    manager: &ConnectionPoolManager,
    page: u32,
    repetition: u32,
    cursor: Option<String>,
    samples: &mut Vec<Value>,
) -> DatasetPage {
    let started = Instant::now();
    let result = dataset::read_page(manager, request(page, cursor))
        .await
        .unwrap();
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    check_page(&result, page);
    samples.push(json!({
        "page": page, "repetition": repetition, "wall_ms": wall_ms,
        "rows": result.page.result.rows.len(), "hasMore": result.has_more,
        "paginationMode": result.pagination_mode, "approxBytes": result.approx_bytes,
        "first_id": (page - 1) * PAGE_SIZE + 1,
        "last_id": ((page - 1) * PAGE_SIZE + result.page.result.rows.len() as u32)
    }));
    result
}

#[tokio::test]
#[ignore = "需显式运行真实 SQLite 百万行性能验收，外层另行采样进程内存"]
async fn sqlite_million_rows_dataset_pages_and_count() {
    let output = output_path();
    let process_id = std::process::id();
    write_report(
        &output,
        &json!({"process_id": process_id, "phase": "fixture"}),
    );
    println!("{}", json!({"process_id": process_id, "phase": "fixture"}));

    let temporary = TemporaryDatabase::new();
    let database_path = temporary.path();
    let fixture_path = database_path.clone();
    let fixture_started = Instant::now();
    tokio::task::spawn_blocking(move || {
        let mut connection = rusqlite::Connection::open(fixture_path).unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute_batch(
                "CREATE TABLE million_rows(id INTEGER PRIMARY KEY NOT NULL,payload TEXT NOT NULL);
                 WITH RECURSIVE numbers(id) AS (
                    VALUES(1) UNION ALL SELECT id+1 FROM numbers WHERE id<1000000
                 )
                 INSERT INTO million_rows(id,payload)
                    SELECT id,replace(hex(zeroblob(64)),'0','x') FROM numbers;",
            )
            .unwrap();
        transaction.commit().unwrap();
    })
    .await
    .unwrap();
    let fixture_ms = fixture_started.elapsed().as_secs_f64() * 1000.0;
    let file_length = std::fs::metadata(&database_path).unwrap().len();
    let manager = ConnectionPoolManager::new(None);
    let config: ConnectionConfig = serde_json::from_value(json!({
        "id": CONNECTION_ID, "name": "P2 百万行性能验收", "driverType": "sqlite",
        "host": "", "port": 0, "user": "", "password": "", "database": database_path
    }))
    .unwrap();
    manager.create_connection(config).await.unwrap();
    write_report(
        &output,
        &json!({"process_id": process_id, "phase": "pages"}),
    );

    let mut samples = Vec::new();
    let last_page = ROW_COUNT.div_ceil(PAGE_SIZE);
    let mut count_report = None;
    for page in [1, 1000, last_page] {
        for repetition in 1..=3 {
            let current = measure_page(&manager, page, repetition, None, &mut samples).await;
            assert_eq!(current.pagination_mode, "offset");
            if current.has_more {
                let cursor = current.next_cursor.clone().expect("整数主键应提供续页游标");
                let next =
                    measure_page(&manager, page + 1, repetition, Some(cursor), &mut samples).await;
                assert_eq!(next.pagination_mode, "keyset");
                retire(&manager, &next);
            }
            if page == last_page && repetition == 3 {
                write_report(
                    &output,
                    &json!({"process_id": process_id, "phase": "count"}),
                );
                let started = Instant::now();
                let count = dataset::count_table(
                    &manager,
                    TableCountRequest {
                        protocol_version: 1,
                        result_id: current.page.context.result_id.clone(),
                        revision: current.page.context.revision,
                        task_id: Some(Uuid::new_v4().to_string()),
                    },
                )
                .await
                .unwrap();
                let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(count.total_rows, ROW_COUNT.to_string());
                count_report = Some(json!({"wall_ms": wall_ms, "rows": ROW_COUNT}));
            }
            retire(&manager, &current);
        }
    }
    manager.disconnect(CONNECTION_ID).await.unwrap();
    let report = json!({
        "process_id": process_id, "phase": "complete", "row_count": ROW_COUNT,
        "payload_bytes": 128, "page_size": PAGE_SIZE,
        "fixture_wall_ms": fixture_ms, "DBfileLength": file_length,
        "samples": samples, "count": count_report
    });
    write_report(&output, &report);
    println!("{report}");
}
