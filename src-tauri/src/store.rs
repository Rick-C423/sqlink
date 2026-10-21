//! 连接配置与密码持久化；任何凭据读取故障都不能触发换钥或覆盖旧密文。
//! 迁移顺序：验证旧密文 → 写入钥匙串 → 回读相同密钥 → 原子持久化指针 → 删除旧材料。

use crate::models::{ConnectionConfig, DatabaseError};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const ENC_PREFIX: &str = "enc:v1:";
const KEYCHAIN_SERVICE: &str = "SQLink";
const LEGACY_KEY_FILE: &str = "key.bin";
const KEY_STATE_FILE: &str = "credentials-state.json";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialHealth {
    pub state: String,
    pub backend: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
struct CredentialIssue {
    state: &'static str,
    message: String,
}

impl CredentialIssue {
    fn new(state: &'static str, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
        }
    }

    fn database_error(&self) -> DatabaseError {
        DatabaseError::Unknown(format!("凭据保护异常（{}）：{}", self.state, self.message))
    }
}

fn io_issue(action: &str, error: io::Error) -> CredentialIssue {
    let state = if error.kind() == io::ErrorKind::PermissionDenied {
        "permission"
    } else {
        "io"
    };
    CredentialIssue::new(state, format!("{action}失败：{error}"))
}

pub struct ConnectionStore {
    path: PathBuf,
    key: Option<[u8; 32]>,
    health: CredentialHealth,
}

impl ConnectionStore {
    pub fn new(dir: PathBuf) -> Result<Self, DatabaseError> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| DatabaseError::Unknown(format!("创建配置目录失败：{e}")))?;
        let _lock = match StoreLock::acquire(&dir) {
            Ok(lock) => lock,
            // 例如目录只读或另一进程长时间占锁：仍能列出已有连接，不能把初始化失败变成空列表。
            Err(issue) => return Ok(Self::from_resolution(&dir, Err(issue))),
        };
        let backend = SystemKeychain::new(&dir);
        Ok(Self::from_resolution(
            &dir,
            resolve_key(&dir, &backend, &RealPersistence),
        ))
    }

    fn from_resolution(dir: &Path, result: Result<KeyMaterial, CredentialIssue>) -> Self {
        let (key, health) = match result {
            Ok(material) => (
                Some(material.key),
                CredentialHealth {
                    state: "ready".into(),
                    backend: Some(material.backend.into()),
                    message: None,
                },
            ),
            Err(issue) => (
                None,
                CredentialHealth {
                    state: issue.state.into(),
                    backend: None,
                    message: Some(issue.message),
                },
            ),
        };
        Self {
            path: dir.join("connections.json"),
            key,
            health,
        }
    }

    /// 故障时仍保留连接元数据读取能力；调用方可展示明确的恢复状态。
    pub fn credential_health(&self) -> CredentialHealth {
        self.health.clone()
    }

    fn require_key(&self) -> Result<&[u8; 32], DatabaseError> {
        self.key.as_ref().ok_or_else(|| {
            DatabaseError::Unknown(format!(
                "凭据保护异常（{}）：{}",
                self.health.state,
                self.health.message.as_deref().unwrap_or("密钥不可用")
            ))
        })
    }

    fn encrypt_password(&self, plain: &str) -> Result<String, DatabaseError> {
        if plain.is_empty() {
            return Ok(String::new());
        }
        let cipher = Aes256Gcm::new_from_slice(self.require_key()?)
            .map_err(|_| DatabaseError::Unknown("密码加密初始化失败".into()))?;
        let nonce_bytes: [u8; 12] = rand::random();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_bytes())
            .map_err(|_| DatabaseError::Unknown("密码加密失败".into()))?;
        Ok(format!(
            "{}{}:{}",
            ENC_PREFIX,
            B64.encode(nonce_bytes),
            B64.encode(ciphertext)
        ))
    }

    fn decrypt_password(&self, stored: &str) -> Result<String, DatabaseError> {
        if !stored.starts_with("enc:") {
            return Ok(stored.to_string());
        }
        decrypt_with_key(self.require_key()?, stored).map_err(|e| e.database_error())
    }

    fn read_all(&self) -> Result<Vec<ConnectionConfig>, DatabaseError> {
        read_configs(&self.path).map_err(|e| e.database_error())
    }

    fn write_all(&self, configs: &[ConnectionConfig]) -> Result<(), DatabaseError> {
        let content = serde_json::to_vec_pretty(configs)
            .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
        RealPersistence
            .write_atomic(&self.path, &content)
            .map_err(|e| e.database_error())
    }

    /// 列表不将密码明文或密文交给前端。
    pub fn list(&self) -> Result<Vec<ConnectionConfig>, DatabaseError> {
        let mut all = self.read_all()?;
        for config in &mut all {
            config.password.clear();
        }
        Ok(all)
    }

    pub fn get_decrypted(&self, id: &str) -> Result<Option<ConnectionConfig>, DatabaseError> {
        for mut config in self.read_all()? {
            if config.id.as_deref() == Some(id) {
                config.password = self.decrypt_password(&config.password)?;
                return Ok(Some(config));
            }
        }
        Ok(None)
    }

    pub fn save(&self, config: &ConnectionConfig) -> Result<(), DatabaseError> {
        let _lock = StoreLock::acquire(self.path.parent().expect("配置路径有父目录"))
            .map_err(|e| e.database_error())?;
        let mut all = self.read_all()?;
        let position = all.iter().position(|c| c.id == config.id);
        let mut to_store = config.clone();
        if config.save_password == Some(false) {
            to_store.password.clear();
        } else if config.password.is_empty() {
            // 列表里的空密码是占位。编辑名称或权限时必须保留原密文，即使钥匙串暂时不可读。
            to_store.password = position
                .map(|idx| all[idx].password.clone())
                .unwrap_or_default();
        } else {
            to_store.password = self.encrypt_password(&config.password)?;
        }
        match position {
            Some(idx) => all[idx] = to_store,
            None => all.push(to_store),
        }
        self.write_all(&all)
    }

    pub fn delete(&self, id: &str) -> Result<(), DatabaseError> {
        let _lock = StoreLock::acquire(self.path.parent().expect("配置路径有父目录"))
            .map_err(|e| e.database_error())?;
        let mut all = self.read_all()?;
        all.retain(|c| c.id.as_deref() != Some(id));
        self.write_all(&all)
    }

    pub fn set_mcp_allowed(&self, id: &str, allowed: bool) -> Result<(), DatabaseError> {
        let _lock = StoreLock::acquire(self.path.parent().expect("配置路径有父目录"))
            .map_err(|e| e.database_error())?;
        let mut all = self.read_all()?;
        let target = all
            .iter_mut()
            .find(|c| c.id.as_deref() == Some(id))
            .ok_or_else(|| DatabaseError::Unknown("连接不存在".into()))?;
        target.mcp_allowed = Some(allowed);
        self.write_all(&all)
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, CredentialIssue> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_issue("读取持久化文件", e)),
    }
}

fn read_configs(path: &Path) -> Result<Vec<ConnectionConfig>, CredentialIssue> {
    match read_optional(path)? {
        None => Ok(vec![]),
        // 空白文件不证明是新配置；避免把截断文件当作空列表覆盖。
        Some(content) => serde_json::from_slice(&content)
            .map_err(|_| CredentialIssue::new("corrupt", "连接配置无法解析；已保留原文件")),
    }
}

fn decrypt_with_key(key: &[u8; 32], stored: &str) -> Result<String, CredentialIssue> {
    let rest = stored
        .strip_prefix(ENC_PREFIX)
        .ok_or_else(|| CredentialIssue::new("corrupt", "密码密文版本无法识别"))?;
    let (nonce_b64, cipher_b64) = rest
        .split_once(':')
        .ok_or_else(|| CredentialIssue::new("corrupt", "密码密文格式错误"))?;
    let nonce_bytes = B64
        .decode(nonce_b64)
        .map_err(|_| CredentialIssue::new("corrupt", "密码随机数编码损坏"))?;
    if nonce_bytes.len() != 12 {
        return Err(CredentialIssue::new(
            "corrupt",
            "密码随机数长度错误；已保留原密文",
        ));
    }
    let ciphertext = B64
        .decode(cipher_b64)
        .map_err(|_| CredentialIssue::new("corrupt", "密码密文编码损坏"))?;
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| CredentialIssue::new("corrupt", "密码密钥长度错误"))?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_ref())
        .map_err(|_| CredentialIssue::new("corrupt", "密码解密校验失败；已保留密钥及原密文"))?;
    String::from_utf8(plain).map_err(|_| CredentialIssue::new("corrupt", "密码明文编码损坏"))
}

fn validate_configs(key: &[u8; 32], configs: &[ConnectionConfig]) -> Result<(), CredentialIssue> {
    for config in configs {
        if config.password.starts_with("enc:") {
            decrypt_with_key(key, &config.password)?;
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct KeyState {
    version: u8,
    backend: String,
    fingerprint: String,
}

struct KeyMaterial {
    key: [u8; 32],
    backend: &'static str,
}

trait CredentialBackend {
    fn get(&self) -> Result<Option<[u8; 32]>, CredentialIssue>;
    fn set(&self, key: &[u8; 32]) -> Result<(), CredentialIssue>;
}

struct SystemKeychain {
    entry: Result<keyring::Entry, CredentialIssue>,
}

impl SystemKeychain {
    fn new(dir: &Path) -> Self {
        // 这些目标以外，keyring 默认是仅驻内存的 mock，绝不能误认为是持久钥匙串。
        let entry = if cfg!(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "ios",
            target_os = "linux",
            target_os = "freebsd",
            target_os = "openbsd"
        )) {
            keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(dir)).map_err(keyring_issue)
        } else {
            Err(CredentialIssue::new(
                "unsupported",
                "当前平台未编译持久化系统钥匙串支持",
            ))
        };
        Self { entry }
    }
}

impl CredentialBackend for SystemKeychain {
    fn get(&self) -> Result<Option<[u8; 32]>, CredentialIssue> {
        let entry = self.entry.as_ref().map_err(Clone::clone)?;
        match entry.get_password() {
            Ok(secret) => B64
                .decode(secret)
                .ok()
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                .map(Some)
                .ok_or_else(|| CredentialIssue::new("corrupt", "钥匙串密钥损坏；不会覆盖该条目")),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(keyring_issue(error)),
        }
    }
    fn set(&self, key: &[u8; 32]) -> Result<(), CredentialIssue> {
        self.entry
            .as_ref()
            .map_err(Clone::clone)?
            .set_password(&B64.encode(key))
            .map_err(keyring_issue)
    }
}

fn keyring_issue(error: keyring::Error) -> CredentialIssue {
    #[cfg(windows)]
    if let Some(platform) = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<keyring::windows::Error>())
    {
        use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NO_SUCH_LOGON_SESSION};
        if platform.0 == ERROR_ACCESS_DENIED.0 {
            return CredentialIssue::new("permission", "系统拒绝访问凭据管理器；不会创建替代密钥");
        }
        if platform.0 == ERROR_NO_SUCH_LOGON_SESSION.0 {
            return CredentialIssue::new(
                "unavailable",
                "当前登录会话无法访问凭据管理器；不会创建替代密钥",
            );
        }
    }
    match error {
        keyring::Error::NoEntry => CredentialIssue::new("missing", "系统钥匙串条目缺失"),
        keyring::Error::NoStorageAccess(_) => {
            CredentialIssue::new("permission", "钥匙串已锁定或无访问权限；不会创建替代密钥")
        }
        keyring::Error::PlatformFailure(_) => {
            CredentialIssue::new("unavailable", "系统钥匙串服务暂不可用；不会创建替代密钥")
        }
        keyring::Error::BadEncoding(_) => {
            CredentialIssue::new("corrupt", "钥匙串密钥编码损坏；不会覆盖该条目")
        }
        keyring::Error::Ambiguous(_) => {
            CredentialIssue::new("conflict", "发现多个匹配的钥匙串条目；需要恢复原有唯一条目")
        }
        keyring::Error::TooLong(_, _) | keyring::Error::Invalid(_, _) => {
            CredentialIssue::new("unsupported", "系统钥匙串参数无效；不会使用文件回退")
        }
        _ => CredentialIssue::new(
            "unavailable",
            "系统钥匙串返回无法识别的故障；不会创建替代密钥",
        ),
    }
}

fn resolve_key(
    dir: &Path,
    backend: &dyn CredentialBackend,
    disk: &dyn Persistence,
) -> Result<KeyMaterial, CredentialIssue> {
    resolve_key_with_support(dir, backend, disk, platform_has_no_keychain())
}

fn resolve_key_with_support(
    dir: &Path,
    backend: &dyn CredentialBackend,
    disk: &dyn Persistence,
    unsupported_platform: bool,
) -> Result<KeyMaterial, CredentialIssue> {
    let key_path = dir.join(LEGACY_KEY_FILE);
    let legacy = match read_optional(&key_path)? {
        None => None,
        Some(bytes) => Some(
            <[u8; 32]>::try_from(bytes)
                .map_err(|_| CredentialIssue::new("corrupt", "key.bin 长度损坏；已保留原文件"))?,
        ),
    };
    let state_path = dir.join(KEY_STATE_FILE);
    let state: Option<KeyState> = read_optional(&state_path)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|_| {
                CredentialIssue::new("corrupt", "凭据迁移记录损坏；已保留所有密钥材料")
            })
        })
        .transpose()?;
    if let Some(state) = &state {
        if state.version != 1 || !matches!(state.backend.as_str(), "keychain" | "file") {
            return Err(CredentialIssue::new(
                "corrupt",
                "凭据迁移记录版本或后端无效",
            ));
        }
    }
    let configs = read_configs(&dir.join("connections.json"))?;
    let fresh = configs.is_empty() && state.is_none() && legacy.is_none();
    let (key, location) = match backend.get() {
        Ok(Some(key)) => {
            if legacy.is_some_and(|old| old != key) {
                return Err(CredentialIssue::new(
                    "conflict",
                    "钥匙串与 key.bin 的密钥冲突；两份材料均已保留",
                ));
            }
            (key, "keychain")
        }
        Ok(None) => {
            // 一旦有持久化钥匙串指针，缺失就属于恢复故障，不能拿旧文件静默回退或换钥。
            if state.as_ref().is_some_and(|s| s.backend == "keychain") {
                return Err(CredentialIssue::new(
                    "missing",
                    "已记录的系统钥匙串密钥缺失；请恢复原条目",
                ));
            }
            let key = match legacy {
                Some(key) => key,
                None if fresh => rand::random::<[u8; 32]>(),
                None => {
                    return Err(CredentialIssue::new(
                        "missing",
                        "现有连接缺少原密钥；不会生成新密钥覆盖",
                    ))
                }
            };
            check_fingerprint(&state, &key)?;
            validate_configs(&key, &configs)?;
            backend.set(&key)?;
            match backend.get()? {
                Some(readback) if readback == key => {}
                _ => {
                    return Err(CredentialIssue::new(
                        "conflict",
                        "钥匙串写入后回读不一致；旧材料已保留",
                    ))
                }
            }
            // 验证回读后的同一把密钥，而不是依赖写入成功的返回值。
            validate_configs(&key, &configs)?;
            (key, "keychain")
        }
        Err(issue) if issue.state == "unsupported" && unsupported_platform => {
            if state.as_ref().is_some_and(|s| s.backend == "keychain") {
                return Err(CredentialIssue::new(
                    "unsupported",
                    "此配置依赖系统钥匙串，当前平台不支持读取",
                ));
            }
            let key = match legacy {
                Some(key) => key,
                None if fresh => {
                    let key: [u8; 32] = rand::random();
                    disk.write_atomic(&key_path, &key)?;
                    key
                }
                None => {
                    return Err(CredentialIssue::new(
                        "missing",
                        "旧密钥文件缺失；不会重新生成",
                    ))
                }
            };
            (key, "file")
        }
        Err(issue) => return Err(issue),
    };
    check_fingerprint(&state, &key)?;
    validate_configs(&key, &configs)?;
    let next = KeyState {
        version: 1,
        backend: location.into(),
        fingerprint: fingerprint(&key),
    };
    if state.as_ref() != Some(&next) {
        let bytes = serde_json::to_vec_pretty(&next)
            .map_err(|_| CredentialIssue::new("io", "无法编码凭据迁移记录"))?;
        disk.write_atomic(&state_path, &bytes)?;
    }
    // 指针已完成持久化。崩溃若发生在这里，重启时会重新校验两份同钥后继续清理。
    if location == "keychain" && legacy.is_some() {
        disk.remove_legacy(&key_path)?;
    }
    Ok(KeyMaterial {
        key,
        backend: location,
    })
}

fn platform_has_no_keychain() -> bool {
    !cfg!(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd"
    ))
}

fn check_fingerprint(state: &Option<KeyState>, key: &[u8; 32]) -> Result<(), CredentialIssue> {
    if state
        .as_ref()
        .is_some_and(|s| s.fingerprint != fingerprint(key))
    {
        return Err(CredentialIssue::new(
            "conflict",
            "密钥与持久化迁移记录不一致；已停止迁移",
        ));
    }
    Ok(())
}

fn fingerprint(key: &[u8; 32]) -> String {
    Sha256::digest(key)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn keychain_account(dir: &Path) -> String {
    let target = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let digest = Sha256::digest(target.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("connections-key-{hex}")
}

/// 仅供既有 MCP 元数据测试清理自己的随机条目，拒绝正式配置目录。
#[cfg(test)]
pub(crate) fn isolated_mcp_test_keychain_entry(dir: &Path) -> Result<keyring::Entry, String> {
    let directory = dir.canonicalize().map_err(|error| error.to_string())?;
    let temporary = std::env::temp_dir()
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("测试目录名称无效")?;
    let suffix = name
        .strip_prefix("sqlink-mcp-whitelist-test-")
        .ok_or("拒绝清理非隔离测试目录的凭据")?;
    if directory.parent() != Some(temporary.as_path()) || uuid::Uuid::parse_str(suffix).is_err() {
        return Err("拒绝清理非随机临时目录的凭据".into());
    }
    let account = keychain_account(&directory);
    eprintln!(
        "MCP 钥匙串隔离条目：平台={}，服务={}，账户={}",
        std::env::consts::OS,
        KEYCHAIN_SERVICE,
        account
    );
    keyring::Entry::new(KEYCHAIN_SERVICE, &account).map_err(|_| "创建隔离测试凭据句柄失败".into())
}

trait Persistence {
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), CredentialIssue>;
    fn remove_legacy(&self, path: &Path) -> Result<(), CredentialIssue>;
}
struct RealPersistence;
impl Persistence for RealPersistence {
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), CredentialIssue> {
        atomic_write(path, bytes, |_| Ok(()))
    }
    fn remove_legacy(&self, path: &Path) -> Result<(), CredentialIssue> {
        std::fs::remove_file(path).map_err(|e| io_issue("清理已迁移的旧密钥", e))?;
        sync_parent(path)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum WriteStage {
    Write,
    Sync,
    Replace,
    ParentSync,
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    mut before: impl FnMut(WriteStage) -> io::Result<()>,
) -> Result<(), CredentialIssue> {
    let parent = path
        .parent()
        .ok_or_else(|| CredentialIssue::new("io", "持久化路径缺少父目录"))?;
    let temporary = parent.join(format!(".sqlink-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|e| io_issue("创建临时文件", e))?;
        before(WriteStage::Write).map_err(|e| io_issue("写入临时文件", e))?;
        file.write_all(bytes)
            .map_err(|e| io_issue("写入临时文件", e))?;
        before(WriteStage::Sync).map_err(|e| io_issue("同步临时文件", e))?;
        file.sync_all().map_err(|e| io_issue("同步临时文件", e))?;
        drop(file);
        before(WriteStage::Replace).map_err(|e| io_issue("原子替换持久化文件", e))?;
        replace_atomic(&temporary, path)?;
        before(WriteStage::ParentSync).map_err(|e| io_issue("同步配置目录", e))?;
        sync_parent(path)
    })();
    // 清理临时文件不影响旧文件；主操作的原始错误必须返回给调用方。
    if temporary.exists() {
        if let Err(cleanup) = std::fs::remove_file(&temporary) {
            if result.is_ok() {
                return Err(io_issue("清理持久化临时文件", cleanup));
            }
            tracing::warn!(error = %cleanup, "清理未提交的配置临时文件失败");
        }
    }
    result
}

#[cfg(windows)]
fn replace_atomic(from: &Path, to: &Path) -> Result<(), CredentialIssue> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // 同目录、同卷替换；写穿标志等待移动写盘，绝不先删除目标文件。
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| CredentialIssue::new("io", format!("原子替换持久化文件失败：{error}")))
}
#[cfg(not(windows))]
fn replace_atomic(from: &Path, to: &Path) -> Result<(), CredentialIssue> {
    std::fs::rename(from, to).map_err(|e| io_issue("原子替换持久化文件", e))
}

fn sync_parent(path: &Path) -> Result<(), CredentialIssue> {
    #[cfg(unix)]
    {
        let parent = File::open(
            path.parent()
                .ok_or_else(|| CredentialIssue::new("io", "缺少父目录"))?,
        )
        .map_err(|e| io_issue("打开配置目录", e))?;
        parent.sync_all().map_err(|e| io_issue("同步配置目录", e))?;
    }
    #[cfg(not(unix))]
    let _ = path; // Windows 的替换由 MOVEFILE_WRITE_THROUGH 同步。
    Ok(())
}

/// 同一目录的读改写/凭据初始化共用进程间锁；进程退出后由操作系统释放，不留陈旧锁。
struct StoreLock {
    _file: File,
}
impl StoreLock {
    fn acquire(dir: &Path) -> Result<Self, CredentialIssue> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(windows)]
        let file = {
            let started = std::time::Instant::now();
            loop {
                match options.open(dir.join(".connections.lock")) {
                    Ok(file) => break file,
                    Err(error)
                        if error.raw_os_error() == Some(32)
                            && started.elapsed() < std::time::Duration::from_secs(5) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => return Err(io_issue("获取配置写锁", error)),
                }
            }
        };
        #[cfg(not(windows))]
        let file = options
            .open(dir.join(".connections.lock"))
            .map_err(|e| io_issue("获取配置写锁", e))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe extern "C" {
                fn flock(fd: std::ffi::c_int, operation: std::ffi::c_int) -> std::ffi::c_int;
            }
            // POSIX 平台 flock(LOCK_EX)，文件关闭时自动释放。
            if unsafe { flock(file.as_raw_fd(), 2) } != 0 {
                return Err(io_issue("获取配置写锁", io::Error::last_os_error()));
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("sqlink-store-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let temporary = std::env::temp_dir().canonicalize().unwrap();
            if self.0.starts_with(&temporary)
                && self
                    .0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("sqlink-store-test-")
            {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[derive(Default)]
    struct MockState {
        key: Option<[u8; 32]>,
        reads: VecDeque<Result<Option<[u8; 32]>, CredentialIssue>>,
        set_error: Option<CredentialIssue>,
        writes: usize,
    }
    #[derive(Default)]
    struct MockKeychain(Mutex<MockState>);
    impl MockKeychain {
        fn with_key(key: [u8; 32]) -> Self {
            Self(Mutex::new(MockState {
                key: Some(key),
                ..Default::default()
            }))
        }
        fn issue(state: &'static str) -> CredentialIssue {
            CredentialIssue::new(state, "注入的凭据故障")
        }
        fn fail_read(state: &'static str) -> Self {
            let mock = Self::default();
            mock.0
                .lock()
                .unwrap()
                .reads
                .push_back(Err(Self::issue(state)));
            mock
        }
    }
    impl CredentialBackend for MockKeychain {
        fn get(&self) -> Result<Option<[u8; 32]>, CredentialIssue> {
            let mut state = self.0.lock().unwrap();
            state.reads.pop_front().unwrap_or(Ok(state.key))
        }
        fn set(&self, key: &[u8; 32]) -> Result<(), CredentialIssue> {
            let mut state = self.0.lock().unwrap();
            state.writes += 1;
            if let Some(error) = &state.set_error {
                return Err(error.clone());
            }
            state.key = Some(*key);
            Ok(())
        }
    }

    struct FaultDisk {
        stage: Option<WriteStage>,
        remove: bool,
    }
    impl Persistence for FaultDisk {
        fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), CredentialIssue> {
            atomic_write(path, bytes, |stage| {
                if self.stage == Some(stage) {
                    Err(io::Error::other("注入的落盘故障"))
                } else {
                    Ok(())
                }
            })
        }
        fn remove_legacy(&self, path: &Path) -> Result<(), CredentialIssue> {
            if self.remove {
                Err(CredentialIssue::new("io", "注入的旧密钥清理故障"))
            } else {
                RealPersistence.remove_legacy(path)
            }
        }
    }

    fn sample_config() -> ConnectionConfig {
        ConnectionConfig {
            id: Some("t1".into()),
            name: "测试连接".into(),
            driver_type: "sqlite".into(),
            host: String::new(),
            port: 0,
            user: String::new(),
            password: "p@ss 测试'quote'\n第二行".into(),
            database: None,
            encrypt: None,
            save_password: Some(true),
            group_name: None,
            color: None,
            mcp_allowed: None,
        }
    }
    fn encrypt_with(key: &[u8; 32], plain: &str) -> String {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let nonce = [7u8; 12];
        let bytes = cipher
            .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
            .unwrap();
        format!("{}{}:{}", ENC_PREFIX, B64.encode(nonce), B64.encode(bytes))
    }
    fn legacy_fixture(dir: &Path, key: &[u8; 32]) -> Vec<u8> {
        std::fs::write(dir.join(LEGACY_KEY_FILE), key).unwrap();
        let mut config = sample_config();
        config.password = encrypt_with(key, &config.password);
        let bytes = serde_json::to_vec_pretty(&vec![config]).unwrap();
        std::fs::write(dir.join("connections.json"), &bytes).unwrap();
        bytes
    }
    fn test_store(dir: &Path, backend: &dyn CredentialBackend) -> ConnectionStore {
        let _lock = StoreLock::acquire(dir).unwrap();
        ConnectionStore::from_resolution(dir, resolve_key(dir, backend, &RealPersistence))
    }
    fn assert_originals(dir: &Path, key: &[u8; 32], configs: &[u8]) {
        assert_eq!(std::fs::read(dir.join(LEGACY_KEY_FILE)).unwrap(), key);
        assert_eq!(
            std::fs::read(dir.join("connections.json")).unwrap(),
            configs
        );
    }

    #[test]
    fn save_decrypt_and_restart_use_the_same_mock_key() {
        let dir = TestDir::new();
        let backend = MockKeychain::default();
        let store = test_store(&dir.0, &backend);
        assert_eq!(store.credential_health().state, "ready");
        store.save(&sample_config()).unwrap();
        let before = std::fs::read(dir.0.join("connections.json")).unwrap();
        assert!(!String::from_utf8_lossy(&before).contains("p@ss"));
        drop(store);
        let restarted = test_store(&dir.0, &backend);
        assert_eq!(
            restarted.get_decrypted("t1").unwrap().unwrap().password,
            sample_config().password
        );
        assert_eq!(restarted.list().unwrap()[0].password, "");
        assert_eq!(backend.0.lock().unwrap().writes, 1);
        assert_eq!(
            std::fs::read(dir.0.join("connections.json")).unwrap(),
            before
        );
    }

    #[test]
    fn every_read_failure_preserves_key_and_ciphertext_without_writes() {
        for code in [
            "locked",
            "unavailable",
            "permission",
            "corrupt",
            "unsupported",
            "io",
        ] {
            let dir = TestDir::new();
            let key = [19; 32];
            let configs = legacy_fixture(&dir.0, &key);
            let backend = MockKeychain::fail_read(code);
            let store = test_store(&dir.0, &backend);
            assert_eq!(store.credential_health().state, code);
            assert_eq!(store.list().unwrap().len(), 1);
            assert!(store.get_decrypted("t1").is_err());
            assert!(store.save(&sample_config()).is_err());
            assert_eq!(backend.0.lock().unwrap().writes, 0);
            assert_originals(&dir.0, &key, &configs);
            assert!(!dir.0.join(KEY_STATE_FILE).exists());
        }
    }

    #[test]
    fn missing_key_with_old_encrypted_records_never_initializes() {
        let dir = TestDir::new();
        let key = [20; 32];
        let configs = legacy_fixture(&dir.0, &key);
        std::fs::remove_file(dir.0.join(LEGACY_KEY_FILE)).unwrap();
        let backend = MockKeychain::default();
        let store = test_store(&dir.0, &backend);
        assert_eq!(store.credential_health().state, "missing");
        assert_eq!(backend.0.lock().unwrap().writes, 0);
        assert_eq!(
            std::fs::read(dir.0.join("connections.json")).unwrap(),
            configs
        );
        assert!(!dir.0.join(LEGACY_KEY_FILE).exists());
    }

    #[test]
    fn legacy_key_migration_validates_all_records_and_retains_ciphertext() {
        let dir = TestDir::new();
        let key = [21; 32];
        let configs = legacy_fixture(&dir.0, &key);
        let backend = MockKeychain::default();
        let store = test_store(&dir.0, &backend);
        assert_eq!(store.credential_health().state, "ready");
        assert_eq!(backend.0.lock().unwrap().key, Some(key));
        assert!(!dir.0.join(LEGACY_KEY_FILE).exists());
        assert_eq!(
            std::fs::read(dir.0.join("connections.json")).unwrap(),
            configs
        );
        assert_eq!(
            store.get_decrypted("t1").unwrap().unwrap().password,
            sample_config().password
        );
        let state: KeyState =
            serde_json::from_slice(&std::fs::read(dir.0.join(KEY_STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state.fingerprint, fingerprint(&key));
        assert_eq!(state.backend, "keychain");
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "ready"
        );
    }

    #[test]
    fn conflict_and_corrupt_legacy_material_never_get_overwritten() {
        let dir = TestDir::new();
        let key = [22; 32];
        let configs = legacy_fixture(&dir.0, &key);
        let backend = MockKeychain::with_key([23; 32]);
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "conflict"
        );
        assert_originals(&dir.0, &key, &configs);
        assert_eq!(backend.0.lock().unwrap().writes, 0);
        std::fs::write(dir.0.join(LEGACY_KEY_FILE), [1, 2, 3]).unwrap();
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "corrupt"
        );
        assert_eq!(
            std::fs::read(dir.0.join(LEGACY_KEY_FILE)).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(backend.0.lock().unwrap().writes, 0);
    }

    #[test]
    fn damaged_nonce_and_one_bad_ciphertext_fail_closed_without_panic() {
        let dir = TestDir::new();
        let key = [24; 32];
        legacy_fixture(&dir.0, &key);
        let mut configs = read_configs(&dir.0.join("connections.json")).unwrap();
        for length in [0, 1, 11, 13, 24] {
            let bad = format!(
                "{}{}:{}",
                ENC_PREFIX,
                B64.encode(vec![0; length]),
                B64.encode([1, 2])
            );
            assert!(decrypt_with_key(&key, &bad).is_err());
        }
        let mut damaged = sample_config();
        damaged.id = Some("damaged".into());
        damaged.password = format!(
            "{}{}:{}",
            ENC_PREFIX,
            B64.encode([1, 2]),
            B64.encode([3, 4])
        );
        configs.push(damaged);
        let bytes = serde_json::to_vec(&configs).unwrap();
        std::fs::write(dir.0.join("connections.json"), &bytes).unwrap();
        let backend = MockKeychain::default();
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "corrupt"
        );
        assert_eq!(backend.0.lock().unwrap().writes, 0);
        assert_originals(&dir.0, &key, &bytes);
    }

    #[test]
    fn keychain_write_and_readback_failures_keep_legacy_key_for_restart() {
        for mode in ["write", "readback", "mismatch"] {
            let dir = TestDir::new();
            let key = [25; 32];
            let configs = legacy_fixture(&dir.0, &key);
            let backend = MockKeychain::default();
            {
                let mut state = backend.0.lock().unwrap();
                match mode {
                    "write" => state.set_error = Some(MockKeychain::issue("unavailable")),
                    "readback" => state
                        .reads
                        .extend([Ok(None), Err(MockKeychain::issue("locked"))]),
                    _ => state.reads.extend([Ok(None), Ok(Some([26; 32]))]),
                }
            }
            assert_ne!(
                test_store(&dir.0, &backend).credential_health().state,
                "ready"
            );
            assert_originals(&dir.0, &key, &configs);
            assert!(!dir.0.join(KEY_STATE_FILE).exists());
            backend.0.lock().unwrap().set_error = None;
            let restarted = test_store(&dir.0, &backend);
            assert_eq!(restarted.credential_health().state, "ready");
            assert_eq!(
                restarted.get_decrypted("t1").unwrap().unwrap().password,
                sample_config().password
            );
        }
    }

    #[test]
    fn every_migration_commit_failure_recovers_on_restart_with_original_key() {
        for stage in [
            WriteStage::Write,
            WriteStage::Sync,
            WriteStage::Replace,
            WriteStage::ParentSync,
        ] {
            let dir = TestDir::new();
            let key = [27; 32];
            let configs = legacy_fixture(&dir.0, &key);
            let backend = MockKeychain::default();
            let disk = FaultDisk {
                stage: Some(stage),
                remove: false,
            };
            let result = resolve_key(&dir.0, &backend, &disk);
            assert!(result.is_err(), "故障点 {stage:?}");
            assert_originals(&dir.0, &key, &configs);
            assert_eq!(backend.0.lock().unwrap().key, Some(key));
            let recovered = test_store(&dir.0, &backend);
            assert_eq!(recovered.credential_health().state, "ready");
            assert_eq!(
                recovered.get_decrypted("t1").unwrap().unwrap().password,
                sample_config().password
            );
            assert_eq!(backend.0.lock().unwrap().writes, 1);
            assert!(!dir.0.join(LEGACY_KEY_FILE).exists());
        }
    }

    #[test]
    fn interrupted_legacy_cleanup_has_durable_pointer_and_retries_safely() {
        let dir = TestDir::new();
        let key = [28; 32];
        let configs = legacy_fixture(&dir.0, &key);
        let backend = MockKeychain::default();
        assert!(resolve_key(
            &dir.0,
            &backend,
            &FaultDisk {
                stage: None,
                remove: true
            }
        )
        .is_err());
        assert!(dir.0.join(KEY_STATE_FILE).exists());
        assert_originals(&dir.0, &key, &configs);
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "ready"
        );
        assert!(!dir.0.join(LEGACY_KEY_FILE).exists());
        assert_eq!(backend.0.lock().unwrap().writes, 1);
    }

    #[test]
    fn persisted_keychain_pointer_prevents_recreation_after_entry_disappears() {
        let dir = TestDir::new();
        let backend = MockKeychain::default();
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "ready"
        );
        let pointer = std::fs::read(dir.0.join(KEY_STATE_FILE)).unwrap();
        backend.0.lock().unwrap().key = None;
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "missing"
        );
        assert_eq!(backend.0.lock().unwrap().writes, 1);
        assert_eq!(std::fs::read(dir.0.join(KEY_STATE_FILE)).unwrap(), pointer);
        assert!(!dir.0.join(LEGACY_KEY_FILE).exists());
    }

    #[test]
    fn pointer_key_conflict_preserves_both_materials() {
        let dir = TestDir::new();
        let backend = MockKeychain::with_key([29; 32]);
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "ready"
        );
        let pointer = std::fs::read(dir.0.join(KEY_STATE_FILE)).unwrap();
        backend.0.lock().unwrap().key = Some([30; 32]);
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "conflict"
        );
        assert_eq!(std::fs::read(dir.0.join(KEY_STATE_FILE)).unwrap(), pointer);
        assert_eq!(backend.0.lock().unwrap().key, Some([30; 32]));
        assert_eq!(backend.0.lock().unwrap().writes, 0);
    }

    #[test]
    fn metadata_updates_during_outage_preserve_password_and_other_records() {
        let dir = TestDir::new();
        let key = [31; 32];
        legacy_fixture(&dir.0, &key);
        let backend = MockKeychain::fail_read("locked");
        let store = test_store(&dir.0, &backend);
        let before = read_configs(&store.path).unwrap()[0].password.clone();
        let mut config = store.list().unwrap().remove(0);
        config.name = "重命名".into();
        store.save(&config).unwrap();
        store.set_mcp_allowed("t1", false).unwrap();
        let saved = read_configs(&store.path).unwrap();
        assert_eq!(saved[0].name, "重命名");
        assert_eq!(saved[0].mcp_allowed, Some(false));
        assert_eq!(saved[0].password, before);
        assert_eq!(std::fs::read(dir.0.join(LEGACY_KEY_FILE)).unwrap(), key);
    }

    #[test]
    fn atomic_write_preserves_old_config_for_write_sync_and_replace_errors() {
        let dir = TestDir::new();
        let path = dir.0.join("connections.json");
        let before = b"[{\"existing\":true}]";
        std::fs::write(&path, before).unwrap();
        for failed in [WriteStage::Write, WriteStage::Sync, WriteStage::Replace] {
            assert!(atomic_write(&path, b"[]", |stage| {
                if stage == failed {
                    Err(io::Error::other("注入的写失败"))
                } else {
                    Ok(())
                }
            })
            .is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        RealPersistence.write_atomic(&path, b"[]").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"[]");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn fallback_is_only_for_explicitly_unsupported_platform_and_propagates_write_errors() {
        for failed in [WriteStage::Write, WriteStage::Sync, WriteStage::Replace] {
            let dir = TestDir::new();
            let backend = MockKeychain::fail_read("unsupported");
            let disk = FaultDisk {
                stage: Some(failed),
                remove: false,
            };
            assert!(resolve_key_with_support(&dir.0, &backend, &disk, true).is_err());
            assert!(!dir.0.join(LEGACY_KEY_FILE).exists());
        }
        let dir = TestDir::new();
        let backend = MockKeychain::fail_read("unsupported");
        let material = resolve_key_with_support(&dir.0, &backend, &RealPersistence, true).unwrap();
        assert_eq!(material.backend, "file");
        assert_eq!(
            std::fs::read(dir.0.join(LEGACY_KEY_FILE)).unwrap(),
            material.key
        );
        let restarted = MockKeychain::fail_read("unsupported");
        assert_eq!(
            resolve_key_with_support(&dir.0, &restarted, &RealPersistence, true)
                .unwrap()
                .key,
            material.key
        );
        let transient = MockKeychain::fail_read("unavailable");
        assert!(resolve_key_with_support(&dir.0, &transient, &RealPersistence, true).is_err());
        assert_eq!(
            std::fs::read(dir.0.join(LEGACY_KEY_FILE)).unwrap(),
            material.key
        );
    }

    #[test]
    fn fallback_directory_sync_failure_restarts_with_the_committed_key() {
        let dir = TestDir::new();
        let backend = MockKeychain::fail_read("unsupported");
        let disk = FaultDisk {
            stage: Some(WriteStage::ParentSync),
            remove: false,
        };
        assert!(resolve_key_with_support(&dir.0, &backend, &disk, true).is_err());
        let committed = std::fs::read(dir.0.join(LEGACY_KEY_FILE)).unwrap();
        assert!(!dir.0.join(KEY_STATE_FILE).exists());
        let restarted = MockKeychain::fail_read("unsupported");
        let recovered =
            resolve_key_with_support(&dir.0, &restarted, &RealPersistence, true).unwrap();
        assert_eq!(recovered.key.as_slice(), committed.as_slice());
        assert!(dir.0.join(KEY_STATE_FILE).exists());
    }

    #[test]
    fn actual_replace_error_preserves_existing_destination() {
        let dir = TestDir::new();
        let destination = dir.0.join("occupied");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("preserved"), "原内容".as_bytes()).unwrap();
        // 用目录占住目标，确保进入真实的平台替换函数而不是只在调用前注入错误。
        assert!(RealPersistence.write_atomic(&destination, b"new").is_err());
        assert_eq!(
            std::fs::read(destination.join("preserved")).unwrap(),
            "原内容".as_bytes()
        );
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_store_instances_do_not_lose_unrelated_updates() {
        let dir = TestDir::new();
        let backend = MockKeychain::default();
        let first = Arc::new(test_store(&dir.0, &backend));
        let second = Arc::new(test_store(&dir.0, &backend));
        let mut tasks = vec![];
        for index in 0..12 {
            let store = if index % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            tasks.push(std::thread::spawn(move || {
                let mut config = sample_config();
                config.id = Some(format!("t{index}"));
                store.save(&config).unwrap();
            }));
        }
        for task in tasks {
            task.join().unwrap();
        }
        assert_eq!(first.list().unwrap().len(), 12);
        for index in 0..12 {
            assert_eq!(
                second
                    .get_decrypted(&format!("t{index}"))
                    .unwrap()
                    .unwrap()
                    .password,
                sample_config().password
            );
        }
    }

    #[test]
    fn truncated_configuration_cannot_be_replaced_by_an_empty_save() {
        let dir = TestDir::new();
        let backend = MockKeychain::default();
        let store = test_store(&dir.0, &backend);
        std::fs::write(&store.path, b" ").unwrap();
        assert!(store.list().is_err());
        assert!(store.save(&sample_config()).is_err());
        assert_eq!(std::fs::read(&store.path).unwrap(), b" ");
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "corrupt"
        );
    }

    #[test]
    fn native_keyring_errors_are_not_classified_as_missing() {
        assert_eq!(keyring_issue(keyring::Error::NoEntry).state, "missing");
        assert_eq!(
            keyring_issue(keyring::Error::BadEncoding(vec![255])).state,
            "corrupt"
        );
        assert_eq!(
            keyring_issue(keyring::Error::NoStorageAccess(Box::new(io::Error::from(
                io::ErrorKind::PermissionDenied
            ))))
            .state,
            "permission"
        );
        assert_eq!(
            keyring_issue(keyring::Error::PlatformFailure(Box::new(io::Error::other(
                "服务故障"
            ))))
            .state,
            "unavailable"
        );
        #[cfg(windows)]
        {
            assert_eq!(
                keyring_issue(keyring::Error::PlatformFailure(Box::new(
                    keyring::windows::Error(5)
                )))
                .state,
                "permission"
            );
            assert_eq!(
                keyring_issue(keyring::Error::NoStorageAccess(Box::new(
                    keyring::windows::Error(1312)
                )))
                .state,
                "unavailable"
            );
        }
    }

    #[test]
    fn mcp_cleanup_helper_rejects_non_isolated_directories_before_keyring_access() {
        let dir = TestDir::new();
        assert!(isolated_mcp_test_keychain_entry(&std::env::temp_dir()).is_err());
        assert!(isolated_mcp_test_keychain_entry(&dir.0).is_err());
        for suffix in ["invalid".to_string(), uuid::Uuid::new_v4().to_string()] {
            let nested = dir.0.join(format!("sqlink-mcp-whitelist-test-{suffix}"));
            std::fs::create_dir(&nested).unwrap();
            assert!(isolated_mcp_test_keychain_entry(&nested).is_err());
        }
    }

    /// 需要系统钥匙串的显式平台回归：使用随机专用服务/账户，绝不接触 SQLink 正式条目。
    #[test]
    #[ignore = "单独运行以验证当前平台系统钥匙串；输出随机专用条目并在结束时清理"]
    fn isolated_system_keychain_roundtrip_and_migration() {
        struct Cleanup(keyring::Entry);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.delete_credential();
            }
        }
        let service = format!("SQLink-isolated-test-{}", uuid::Uuid::new_v4());
        let account = format!("isolated-{}", uuid::Uuid::new_v4());
        eprintln!(
            "系统钥匙串隔离测试：平台={}，服务={}，账户={}",
            std::env::consts::OS,
            service,
            account
        );
        let entry = Cleanup(keyring::Entry::new(&service, &account).unwrap());
        assert!(matches!(
            entry.0.get_password(),
            Err(keyring::Error::NoEntry)
        ));
        let backend = SystemKeychain {
            entry: Ok(keyring::Entry::new(&service, &account).unwrap()),
        };
        let dir = TestDir::new();
        let key = [32; 32];
        let configs = legacy_fixture(&dir.0, &key);
        assert_eq!(
            test_store(&dir.0, &backend).credential_health().state,
            "ready"
        );
        assert_eq!(backend.get().unwrap(), Some(key));
        assert_eq!(
            test_store(&dir.0, &backend)
                .get_decrypted("t1")
                .unwrap()
                .unwrap()
                .password,
            sample_config().password
        );
        assert_eq!(
            std::fs::read(dir.0.join("connections.json")).unwrap(),
            configs
        );
        entry.0.delete_credential().unwrap();
        assert!(matches!(
            entry.0.get_password(),
            Err(keyring::Error::NoEntry)
        ));
        eprintln!("隔离系统钥匙串条目已删除");
    }
}
