//! 连接配置的本地持久化存储。
//! 文件: {app_config_dir}/connections.json
//! 密码字段以 AES-256-GCM 加密后 base64 存储（"enc:v1:<nonce>:<cipher>"），
//! 密钥存于同目录 key.bin（首次运行随机生成）。
//! 注意：密钥与数据同机存放，只能防"随手拷贝"，不防针对性攻击；后续可迁移到系统钥匙串。

use std::path::PathBuf;
use aes_gcm::{Aes256Gcm, Nonce};
use aes_gcm::aead::{Aead, KeyInit};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use crate::models::{ConnectionConfig, DatabaseError};

const ENC_PREFIX: &str = "enc:v1:";

pub struct ConnectionStore {
    path: PathBuf,
    key: [u8; 32],
}

impl ConnectionStore {
    pub fn new(dir: PathBuf) -> Result<Self, DatabaseError> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| DatabaseError::Unknown(format!("创建配置目录失败: {}", e)))?;

        let key_path = dir.join("key.bin");
        let key: [u8; 32] = if key_path.exists() {
            let bytes = std::fs::read(&key_path)
                .map_err(|e| DatabaseError::Unknown(format!("读取密钥失败: {}", e)))?;
            if bytes.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&bytes);
                k
            } else {
                // 密钥文件损坏：重新生成（旧数据将无法解密，需重新保存连接）
                let k: [u8; 32] = rand::random();
                std::fs::write(&key_path, k).ok();
                k
            }
        } else {
            let k: [u8; 32] = rand::random();
            std::fs::write(&key_path, k)
                .map_err(|e| DatabaseError::Unknown(format!("写入密钥失败: {}", e)))?;
            k
        };

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
        Ok(format!("{}{}:{}", ENC_PREFIX, B64.encode(nonce_bytes), B64.encode(ciphertext)))
    }

    fn decrypt_password(&self, stored: &str) -> Result<String, DatabaseError> {
        if let Some(rest) = stored.strip_prefix(ENC_PREFIX) {
            let (nonce_b64, cipher_b64) = rest.split_once(':')
                .ok_or_else(|| DatabaseError::Unknown("密码密文格式错误".into()))?;
            let nonce_bytes = B64.decode(nonce_b64)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
            let ciphertext = B64.decode(cipher_b64)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
            let cipher = Aes256Gcm::new_from_slice(&self.key)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))?;
            let plain = cipher
                .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_ref())
                .map_err(|_| DatabaseError::Unknown("密码解密失败（密钥可能已更换）".into()))?;
            String::from_utf8(plain)
                .map_err(|e| DatabaseError::Unknown(e.to_string()))
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
