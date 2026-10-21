//! 有界缓冲的原子导出：写入同目录临时文件，完整落盘后才替换目标。
//!
//! 路径检查用于拒绝已有链接和特殊文件；不承诺抵御能并发篡改父目录的恶意进程。
//! 正常取消只需丢弃本对象；进程被强制终止时可能留下临时文件，但不会发布半成品。

use crate::models::DatabaseError;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

const BUFFER_CAPACITY: usize = 64 * 1024;

pub struct AtomicExportFile {
    target: PathBuf,
    temporary: PathBuf,
    writer: Option<BufWriter<File>>,
    bytes: u64,
    failed: bool,
    committed: bool,
    #[cfg(test)]
    fail_at: Option<FailureStage>,
}

impl AtomicExportFile {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path.file_name().is_none() {
            return Err(DatabaseError::Unknown("导出路径必须包含文件名".into()));
        }
        let target = std::path::absolute(path).map_err(|e| error("解析导出路径", e))?;
        validate_paths(&target)?;
        let parent = target.parent().expect("绝对文件路径具有父目录");
        for _ in 0..16 {
            let temporary = parent.join(format!(".sqlink-export-{}.tmp", uuid::Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(0);
            }
            match options.open(&temporary) {
                Ok(file) => {
                    return Ok(Self {
                        target,
                        temporary,
                        writer: Some(BufWriter::with_capacity(BUFFER_CAPACITY, file)),
                        bytes: 0,
                        failed: false,
                        committed: false,
                        #[cfg(test)]
                        fail_at: None,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(error("创建导出临时文件", e)),
            }
        }
        Err(DatabaseError::Unknown(
            "创建导出临时文件失败：随机名称连续冲突".into(),
        ))
    }

    /// 返回成功交给本对象的字节数，尚未提交时这些字节仍属于临时文件。
    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    pub fn commit(mut self) -> Result<(), DatabaseError> {
        self.flush().map_err(|e| error("刷新导出临时文件", e))?;
        #[cfg(test)]
        self.inject(FailureStage::Sync)
            .map_err(|e| error("同步导出临时文件", e))?;
        self.writer
            .as_ref()
            .expect("提交前文件保持打开")
            .get_ref()
            .sync_all()
            .map_err(|e| error("同步导出临时文件", e))?;

        // 必须关闭句柄后替换；into_parts 避免析构时再次隐式刷新。
        self.close_without_flush();
        validate_paths(&self.target)?;
        #[cfg(test)]
        self.inject(FailureStage::Replace)
            .map_err(|e| error("原子替换导出文件", e))?;
        replace_atomic(&self.temporary, &self.target).map_err(|e| error("原子替换导出文件", e))?;
        self.committed = true;

        // 替换已完成，后续持久性错误不能宣称原文件仍在，也不能尝试回滚。
        #[cfg(test)]
        self.inject(FailureStage::ParentSync)
            .map_err(post_replace_error)?;
        sync_parent(&self.target).map_err(post_replace_error)
    }

    fn close_without_flush(&mut self) {
        if let Some(writer) = self.writer.take() {
            let (file, _unwritten) = writer.into_parts();
            drop(file);
        }
    }

    fn require_writable(&self) -> io::Result<()> {
        if self.failed || self.writer.is_none() {
            Err(io::Error::other(
                "导出写入已经失败或文件已经关闭，不能继续提交",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn inject(&mut self, stage: FailureStage) -> io::Result<()> {
        if self.fail_at == Some(stage) {
            self.failed = true;
            Err(io::Error::other(format!("测试注入故障：{stage:?}")))
        } else {
            Ok(())
        }
    }
}

impl Write for AtomicExportFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.require_writable()?;
        #[cfg(test)]
        self.inject(FailureStage::Write)?;
        match self
            .writer
            .as_mut()
            .expect("写入前已检查句柄")
            .write(buffer)
        {
            Ok(count) => {
                self.bytes = self.bytes.saturating_add(count as u64);
                Ok(count)
            }
            Err(e) => {
                self.failed = true;
                Err(io::Error::new(
                    e.kind(),
                    format!("写入导出临时文件失败：{e}"),
                ))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.require_writable()?;
        #[cfg(test)]
        self.inject(FailureStage::Flush)?;
        if let Err(e) = self.writer.as_mut().expect("刷新前已检查句柄").flush() {
            self.failed = true;
            return Err(io::Error::new(
                e.kind(),
                format!("刷新导出临时文件失败：{e}"),
            ));
        }
        Ok(())
    }
}

impl Drop for AtomicExportFile {
    fn drop(&mut self) {
        self.close_without_flush();
        if !self.committed {
            // 清理是尽力而为；取消或错误不能覆盖原来的失败原因。
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

fn error(action: &str, e: io::Error) -> DatabaseError {
    DatabaseError::Unknown(format!("{action}失败：{e}"))
}

fn post_replace_error(e: io::Error) -> DatabaseError {
    DatabaseError::Unknown(format!(
        "EXPORT_FILE_REPLACED_DURABILITY_UNKNOWN: 导出文件已替换，但目录同步失败，断电后的持久性尚未确认：{e}"
    ))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // 同时拒绝目录联接、挂载点及其他重解析点。
        return metadata.file_attributes() & 0x400 != 0;
    }
    #[cfg(not(windows))]
    false
}

fn validate_paths(target: &Path) -> Result<(), DatabaseError> {
    let parent = target
        .parent()
        .ok_or_else(|| DatabaseError::Unknown("导出路径缺少父目录".into()))?;
    for directory in parent.ancestors() {
        let metadata = fs::symlink_metadata(directory).map_err(|e| error("检查导出父目录", e))?;
        if is_link(&metadata) || !metadata.is_dir() {
            return Err(DatabaseError::Unknown(
                "导出父路径必须是已有普通目录，不能包含符号链接或重解析点".into(),
            ));
        }
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_link(&metadata) || !metadata.is_file() => Err(DatabaseError::Unknown(
            "导出目标必须是普通文件，不能覆盖目录、符号链接或重解析点".into(),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(error("检查导出目标", e)),
    }
}

#[cfg(windows)]
fn replace_atomic(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // 同目录同卷原子替换，绝不预先删除目标。
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))
}

#[cfg(not(windows))]
fn replace_atomic(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

fn sync_parent(target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(target.parent().expect("绝对文件路径具有父目录"))?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = target; // Windows 使用 MOVEFILE_WRITE_THROUGH。
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureStage {
    Write,
    Flush,
    Sync,
    Replace,
    ParentSync,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            // 先解析测试环境临时根目录的链接，避免 /var 等系统别名影响路径检查。
            let root = fs::canonicalize(std::env::temp_dir()).unwrap();
            let path = root.join(format!("sqlink-atomic-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn target(&self) -> PathBuf {
            self.0.join("导出.csv")
        }

        fn assert_only_target(&self) {
            let entries: Vec<_> = fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            assert_eq!(entries, vec![self.target()]);
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn commit_replaces_existing_file_only_after_complete_write() {
        let directory = TestDirectory::new();
        let target = directory.target();
        fs::write(&target, b"old").unwrap();
        let mut export = AtomicExportFile::create(&target).unwrap();
        let content = vec![b'x'; BUFFER_CAPACITY * 3 + 7];
        export.write_all(&content).unwrap();
        assert_eq!(export.bytes_written(), content.len() as u64);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        export.flush().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        export.commit().unwrap();
        assert_eq!(fs::read(target).unwrap(), content);
        directory.assert_only_target();
    }

    #[test]
    fn commit_creates_new_file_and_supports_empty_export() {
        for content in [b"new".as_slice(), b"".as_slice()] {
            let directory = TestDirectory::new();
            let mut export = AtomicExportFile::create(directory.target()).unwrap();
            export.write_all(content).unwrap();
            assert!(!directory.target().exists());
            export.commit().unwrap();
            assert_eq!(fs::read(directory.target()).unwrap(), content);
            directory.assert_only_target();
        }
    }

    #[test]
    fn drop_cancels_export_and_removes_temporary_file() {
        for existing in [false, true] {
            let directory = TestDirectory::new();
            if existing {
                fs::write(directory.target(), b"old").unwrap();
            }
            let mut export = AtomicExportFile::create(directory.target()).unwrap();
            let temporary = export.temporary.clone();
            export.write_all(b"unfinished").unwrap();
            drop(export);
            assert!(!temporary.exists());
            if existing {
                assert_eq!(fs::read(directory.target()).unwrap(), b"old");
                directory.assert_only_target();
            } else {
                assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
            }
        }
    }

    #[test]
    fn injected_write_flush_sync_and_replace_failures_preserve_original() {
        for stage in [
            FailureStage::Write,
            FailureStage::Flush,
            FailureStage::Sync,
            FailureStage::Replace,
        ] {
            let directory = TestDirectory::new();
            fs::write(directory.target(), b"old").unwrap();
            let mut export = AtomicExportFile::create(directory.target()).unwrap();
            export.write_all(b"prefix").unwrap();
            export.fail_at = Some(stage);
            if stage == FailureStage::Write {
                assert!(export.write_all(b"suffix").is_err());
                // 即使调用者忽略写错误，也不允许提交不完整结果。
            }
            assert!(export.commit().is_err(), "阶段 {stage:?} 必须失败");
            assert_eq!(fs::read(directory.target()).unwrap(), b"old");
            directory.assert_only_target();
        }
    }

    #[test]
    fn ignored_explicit_flush_failure_cannot_later_commit() {
        let directory = TestDirectory::new();
        fs::write(directory.target(), b"old").unwrap();
        let mut export = AtomicExportFile::create(directory.target()).unwrap();
        export.write_all(b"new").unwrap();
        export.fail_at = Some(FailureStage::Flush);
        assert!(export.flush().is_err());
        export.fail_at = None;
        assert!(export.commit().is_err());
        assert_eq!(fs::read(directory.target()).unwrap(), b"old");
        directory.assert_only_target();
    }

    #[test]
    fn directory_sync_failure_reports_that_replacement_already_happened() {
        let directory = TestDirectory::new();
        fs::write(directory.target(), b"old").unwrap();
        let mut export = AtomicExportFile::create(directory.target()).unwrap();
        export.write_all(b"new").unwrap();
        export.fail_at = Some(FailureStage::ParentSync);
        let message = export.commit().unwrap_err().to_string();
        assert!(message.contains("已替换"));
        assert!(message.contains("持久性尚未确认"));
        assert_eq!(fs::read(directory.target()).unwrap(), b"new");
        directory.assert_only_target();
    }

    #[test]
    fn invalid_paths_do_not_create_temporary_files() {
        let directory = TestDirectory::new();
        assert!(AtomicExportFile::create("").is_err());
        assert!(AtomicExportFile::create(&directory.0).is_err());
        assert!(AtomicExportFile::create(directory.0.join("missing/file.csv")).is_err());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[test]
    fn directory_created_at_target_before_commit_is_preserved() {
        let directory = TestDirectory::new();
        let mut export = AtomicExportFile::create(directory.target()).unwrap();
        export.write_all(b"new").unwrap();
        fs::create_dir(directory.target()).unwrap();
        assert!(export.commit().is_err());
        assert!(directory.target().is_dir());
        directory.assert_only_target();
    }

    #[test]
    fn subprocess_exit_without_drop_never_publishes_partial_export() {
        let directory = TestDirectory::new();
        fs::write(directory.target(), b"old").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "atomic_export::tests::subprocess_exit_before_commit_helper",
            ])
            .env("SQLINK_ATOMIC_EXPORT_TEST_DIRECTORY", &directory.0)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(73));
        assert_eq!(fs::read(directory.target()).unwrap(), b"old");
        // 未执行 Drop 的退出会留下临时文件；这与正常取消的清理保证不同。
        let leftovers: Vec<_> = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path != &directory.target())
            .collect();
        assert_eq!(leftovers.len(), 1);
        assert!(leftovers[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".sqlink-export-"));
    }

    #[test]
    #[ignore = "仅供父测试在独立子进程中调用，退出时故意跳过析构"]
    fn subprocess_exit_before_commit_helper() {
        let Some(directory) = std::env::var_os("SQLINK_ATOMIC_EXPORT_TEST_DIRECTORY") else {
            return;
        };
        let mut export =
            AtomicExportFile::create(PathBuf::from(directory).join("导出.csv")).unwrap();
        export.write_all(&vec![b'x'; BUFFER_CAPACITY * 3]).unwrap();
        export.flush().unwrap();
        // exit 不展开栈、不执行 AtomicExportFile::drop，模拟提交前进程突然消失。
        std::process::exit(73);
    }

    #[cfg(windows)]
    #[test]
    fn real_windows_replace_failure_preserves_open_target() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = TestDirectory::new();
        fs::write(directory.target(), b"old").unwrap();
        let mut export = AtomicExportFile::create(directory.target()).unwrap();
        export.write_all(b"new").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(directory.target())
            .unwrap();
        assert!(export.commit().is_err());
        drop(lock);
        assert_eq!(fs::read(directory.target()).unwrap(), b"old");
        directory.assert_only_target();
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_targets_and_parent_directories_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = TestDirectory::new();
        let real = directory.0.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("old.csv"), b"old").unwrap();
        symlink(real.join("old.csv"), directory.target()).unwrap();
        assert!(AtomicExportFile::create(directory.target()).is_err());
        let alias = directory.0.join("alias");
        symlink(&real, &alias).unwrap();
        assert!(AtomicExportFile::create(alias.join("new.csv")).is_err());
        assert_eq!(fs::read(real.join("old.csv")).unwrap(), b"old");
        assert_eq!(fs::read_dir(&real).unwrap().count(), 1);
    }
}
