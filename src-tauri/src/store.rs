//! 连接配置的本地持久化存储。
//! 文件: {app_config_dir}/connections.json
//! 密码字段以 AES-256-GCM 加密后 base64 存储（"enc:v1:<nonce>:<cipher>"）。
//!
//! 加密密钥优先保存在系统钥匙串（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret
//! Service），条目按配置目录路径派生，多套安装互不共享。旧版本升级路径：首次运行
//! 检测到钥匙串无条目而已存在 key.bin 时，把密钥原样搬入钥匙串后删除该文件，密文
//! 无需重新加密。钥匙串不可用（如无桌面环境的 Linux）时回退为 key.bin 同目录存放。

use crate::models::{ConnectionConfig, DatabaseError};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const ENC_PREFIX: &str = "enc:v1:";
const KEYCHAIN_SERVICE: &str = "SQLink";
const LEGACY_KEY_FILE: &str = "key.bin";

pub struct ConnectionStore {
    path: PathBuf,
    key: [u8; 32],
}

impl ConnectionStore {
    pub fn new(dir: PathBuf) -> Result<Self, DatabaseError> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| DatabaseError::Unknown(format!("创建配置目录失败: {}", e)))?;

        let key = load_or_create_key(&dir);

        Ok(Self {
            path: dir.join("connections.json"),
            key,
        })
    }

    fn encrypt_password(&self, plain: &str) -> Result<String, DatabaseError> {
        if plain.is_empty() {
            return Ok(String::new());
        }
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
        let nonce_bytes: [u8; 12] = rand::random();
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_bytes())
            .map_err(|e| DatabaseError::Unknown(format!("密码加密失败: {}", e)))?;
        Ok(format!(
            "{}{}:{}",
            ENC_PREFIX,
            B64.encode(nonce_bytes),
            B64.encode(ciphertext)
        ))
    }

    fn decrypt_password(&self, stored: &str) -> Result<String, DatabaseError> {
        if let Some(rest) = stored.strip_prefix(ENC_PREFIX) {
            let (nonce_b64, cipher_b64) = rest
                .split_once(':')
                .ok_or_else(|| DatabaseError::Unknown("密码密文格式错误".into()))?;
            let nonce_bytes = B64
                .decode(nonce_b64)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
            let ciphertext = B64
                .decode(cipher_b64)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
            let cipher = Aes256Gcm::new_from_slice(&self.key)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
            let plain = cipher
                .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_ref())
                .map_err(|_| DatabaseError::Unknown("密码解密失败（密钥可能已更换）".into()))?;
            String::from_utf8(plain).map_err(|e| DatabaseError::Unknown(e.to_string()))
        } else {
            // 未加密（旧数据或空密码）
            Ok(stored.to_string())
        }
    }

    fn read_all(&self) -> Result<Vec<ConnectionConfig>, DatabaseError> {
        if !self.path.exists() {
            return Ok(vec![]);
        }
        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| DatabaseError::Unknown(format!("读取连接配置失败: {}", e)))?;
        if content.trim().is_empty() {
            return Ok(vec![]);
        }
        serde_json::from_str(&content)
            .map_err(|e| DatabaseError::Unknown(format!("解析连接配置失败: {}", e)))
    }

    fn write_all(&self, configs: &[ConnectionConfig]) -> Result<(), DatabaseError> {
        let content = serde_json::to_string_pretty(configs)
            .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
        std::fs::write(&self.path, content)
            .map_err(|e| DatabaseError::Unknown(format!("写入连接配置失败: {}", e)))
    }

    /// 列出已保存连接（密码掩码为空，避免明文/密文离开后端）
    pub fn list(&self) -> Result<Vec<ConnectionConfig>, DatabaseError> {
        let mut all = self.read_all()?;
        for c in &mut all {
            c.password = String::new();
        }
        Ok(all)
    }

    /// 按 id 取回完整配置（密码已解密，仅供后端建连使用）
    pub fn get_decrypted(&self, id: &str) -> Result<Option<ConnectionConfig>, DatabaseError> {
        let all = self.read_all()?;
        for mut c in all {
            if c.id.as_deref() == Some(id) {
                c.password = self.decrypt_password(&c.password)?;
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// 保存/更新（按 id upsert）；save_password == false 时密码不落盘
    pub fn save(&self, config: &ConnectionConfig) -> Result<(), DatabaseError> {
        let mut all = self.read_all()?;
        let mut to_store = config.clone();
        if config.save_password == Some(false) {
            to_store.password = String::new();
        } else {
            to_store.password = self.encrypt_password(&config.password)?;
        }

        match all.iter().position(|c| c.id == to_store.id) {
            Some(idx) => all[idx] = to_store,
            None => all.push(to_store),
        }
        self.write_all(&all)
    }

    pub fn delete(&self, id: &str) -> Result<(), DatabaseError> {
        let mut all = self.read_all()?;
        all.retain(|c| c.id.as_deref() != Some(id));
        self.write_all(&all)
    }

    /// 仅更新连接的 MCP 访问开关：保留密文密码原样，不触碰活动连接。
    pub fn set_mcp_allowed(&self, id: &str, allowed: bool) -> Result<(), DatabaseError> {
        let mut all = self.read_all()?;
        let target = all
            .iter_mut()
            .find(|c| c.id.as_deref() == Some(id))
            .ok_or_else(|| DatabaseError::Unknown("连接不存在".into()))?;
        target.mcp_allowed = Some(allowed);
        self.write_all(&all)
    }
}

/// 加密密钥的加载与创建：优先系统钥匙串，失败回退 key.bin。
fn load_or_create_key(dir: &Path) -> [u8; 32] {
    let key_path = dir.join(LEGACY_KEY_FILE);
    match load_from_keychain(dir, &key_path) {
        Some(key) => key,
        None => legacy_file_key(&key_path),
    }
}

/// 从系统钥匙串取密钥；条目缺失时把 key.bin（如有）原样迁移进去。
/// 返回 None 表示钥匙串不可用或读写失败，调用方应走 key.bin 回退路径。
fn load_from_keychain(dir: &Path, key_path: &Path) -> Option<[u8; 32]> {
    let entry = match keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(dir)) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::debug!(error = %e, "系统钥匙串不可用");
            return None;
        }
    };

    match entry.get_password() {
        Ok(secret) => {
            if let Some(key) = decode_key(&secret) {
                // 迁移的收尾：钥匙串已生效，清理遗留的 key.bin（删除失败不阻塞）
                if key_path.exists() {
                    std::fs::remove_file(key_path).ok();
                }
                return Some(key);
            }
            tracing::warn!("钥匙串中的密钥无法解析，尝试重建");
        }
        Err(keyring::Error::NoEntry) => {}
        Err(e) => {
            tracing::warn!(error = %e, "读取系统钥匙串失败");
            return None;
        }
    }

    // 条目缺失或损坏：迁移现有 key.bin，或生成新密钥
    let (key, had_legacy_file) = read_or_generate_key(key_path);
    match entry.set_password(&B64.encode(key)) {
        Ok(()) => {
            if had_legacy_file {
                std::fs::remove_file(key_path).ok();
                tracing::info!("加密密钥已迁移到系统钥匙串，已删除 key.bin");
            }
            Some(key)
        }
        Err(e) => {
            tracing::warn!(error = %e, "写入系统钥匙串失败，密钥回退到 key.bin");
            None
        }
    }
}

/// key.bin 回退路径：读取现有密钥；不存在或损坏时生成并写入。
fn legacy_file_key(key_path: &Path) -> [u8; 32] {
    if let Some(key) = read_key_file(key_path) {
        return key;
    }
    let key: [u8; 32] = rand::random();
    std::fs::write(key_path, key).ok();
    key
}

/// 迁移场景：读取 key.bin（有效则返回，损坏则重新生成）；不落盘。
/// 返回 (密钥, 是否存在可清理的旧文件)。
fn read_or_generate_key(key_path: &Path) -> ([u8; 32], bool) {
    match read_key_file(key_path) {
        Some(key) => (key, true),
        None => {
            if key_path.exists() {
                tracing::warn!("key.bin 已损坏，重新生成密钥（旧密文将无法解密，需重新保存连接）");
                (rand::random(), true)
            } else {
                (rand::random(), false)
            }
        }
    }
}

fn read_key_file(key_path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(key_path).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Some(key)
}

fn decode_key(secret: &str) -> Option<[u8; 32]> {
    B64.decode(secret)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
}

/// 钥匙串条目按配置目录路径派生：一套安装一个条目，互不覆盖。
fn keychain_account(dir: &Path) -> String {
    let target = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let digest = Sha256::digest(target.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    format!("connections-key-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sqlink-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_config() -> ConnectionConfig {
        ConnectionConfig {
            id: Some("t1".into()),
            name: "测试连接".into(),
            driver_type: "sqlite".into(),
            host: String::new(),
            port: 0,
            user: String::new(),
            password: "p@ss 测试'quote'\nline2".into(),
            database: None,
            encrypt: None,
            save_password: Some(true),
            group_name: None,
            color: None,
            mcp_allowed: None,
        }
    }

    /// 用指定密钥手工加密（模拟旧版本落盘的密文）
    fn encrypt_with(key: &[u8; 32], plain: &str) -> String {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        let nonce_bytes = [7u8; 12];
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_bytes())
            .unwrap();
        format!(
            "{}{}:{}",
            ENC_PREFIX,
            B64.encode(nonce_bytes),
            B64.encode(ciphertext)
        )
    }

    #[test]
    fn save_and_decrypt_roundtrip() {
        let dir = temp_dir();
        // 目录删除后 canonicalize 结果会变，条目句柄要先建好
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(&dir)).ok();
        let store = ConnectionStore::new(dir.clone()).unwrap();
        store.save(&sample_config()).unwrap();

        let got = store.get_decrypted("t1").unwrap().unwrap();
        assert_eq!(got.password, "p@ss 测试'quote'\nline2", "解密应还原原文");

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].password, "", "列表不应回传密码");

        std::fs::remove_dir_all(dir).ok();
        if let Some(entry) = entry {
            entry.delete_credential().ok();
        }
    }

    #[test]
    fn legacy_key_bin_migrates_without_reencryption() {
        let dir = temp_dir();
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(&dir)).ok();
        let legacy_key: [u8; 32] = rand::random();
        std::fs::write(dir.join(LEGACY_KEY_FILE), legacy_key).unwrap();

        // 模拟旧版本数据：密文用 key.bin 的密钥加密
        let config_json = format!(
            r#"[{{"id":"t1","name":"旧连接","driverType":"mysql","host":"127.0.0.1","port":3306,"user":"u","password":"{}","savePassword":true}}]"#,
            encrypt_with(&legacy_key, "旧版本密码")
        );
        std::fs::write(dir.join("connections.json"), config_json).unwrap();

        // 新版本初始化：密钥应被搬到钥匙串（或钥匙串不可用时保留 key.bin），
        // 但无论落在哪，密文都必须仍能解开——迁移不得更换密钥
        let store = ConnectionStore::new(dir.clone()).unwrap();
        let got = store.get_decrypted("t1").unwrap().unwrap();
        assert_eq!(got.password, "旧版本密码", "迁移后旧密文应仍可解密");

        // 钥匙串可用时不得遗留 key.bin
        if let Some(entry) = &entry {
            if entry.get_password().is_ok() {
                assert!(
                    !dir.join(LEGACY_KEY_FILE).exists(),
                    "密钥迁入钥匙串后 key.bin 应被删除"
                );
            }
        }

        std::fs::remove_dir_all(dir).ok();
        if let Some(entry) = entry {
            entry.delete_credential().ok();
        }
    }
}
