//! 查询历史的本地持久化存储。
//! 文件: {app_config_dir}/history.jsonl（每行一个 JSON，追加写入）
//! 容量控制：超过 TRIM_AT 条时重写文件，只保留最近 KEEP 条。

use crate::models::{DatabaseError, HistoryEntry};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

const KEEP: usize = 500;
const TRIM_AT: usize = 600;

pub struct HistoryStore {
    path: PathBuf,
}

impl HistoryStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            path: dir.join("history.jsonl"),
        }
    }

    fn read_all(&self) -> Vec<HistoryEntry> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .filter_map(|line| line.ok())
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    fn write_all(&self, entries: &[HistoryEntry]) -> Result<(), DatabaseError> {
        let mut out = String::new();
        for e in entries {
            out.push_str(
                &serde_json::to_string(e).map_err(|e| DatabaseError::Unknown(e.to_string()))?,
            );
            out.push('\n');
        }
        std::fs::write(&self.path, out)
            .map_err(|e| DatabaseError::Unknown(format!("写入查询历史失败: {}", e)))
    }

    /// 追加一条；超量时重写瘦身
    pub fn append(&self, entry: HistoryEntry) -> Result<(), DatabaseError> {
        let mut line =
            serde_json::to_string(&entry).map_err(|e| DatabaseError::Unknown(e.to_string()))?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| DatabaseError::Unknown(format!("写入查询历史失败: {}", e)))?;
        file.write_all(line.as_bytes())
            .map_err(|e| DatabaseError::Unknown(format!("写入查询历史失败: {}", e)))?;

        // 粗略按行数判断是否需要瘦身（读一遍而已，文件很小）
        let all = self.read_all();
        if all.len() > TRIM_AT {
            let keep_from = all.len() - KEEP;
            self.write_all(&all[keep_from..])?;
        }
        Ok(())
    }

    /// 按连接查询，最新的在前
    pub fn list(&self, connection_id: &str, limit: usize) -> Vec<HistoryEntry> {
        let mut entries: Vec<HistoryEntry> = self
            .read_all()
            .into_iter()
            .filter(|e| e.connection_id == connection_id)
            .collect();
        entries.reverse();
        entries.truncate(limit);
        entries
    }

    pub fn delete(&self, id: &str) -> Result<(), DatabaseError> {
        let all = self.read_all();
        let kept: Vec<_> = all.into_iter().filter(|e| e.id != id).collect();
        self.write_all(&kept)
    }

    pub fn clear(&self, connection_id: &str) -> Result<(), DatabaseError> {
        let all = self.read_all();
        let kept: Vec<_> = all
            .into_iter()
            .filter(|e| e.connection_id != connection_id)
            .collect();
        self.write_all(&kept)
    }
}
