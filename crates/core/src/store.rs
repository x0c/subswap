//! 凭证仓库抽象。Token、refresh_token 等敏感字段一律走系统 keyring；
//! 元数据（label、过期时间、优先级等）走 [`crate::paths`] 下的明文 toml/json。
//!
//! 命名约定：
//! - service: `subswap`
//! - key:     `{provider_id}:{account_id}:{field}`，例如 `claude:alice@example.com:access_token`

use crate::error::{Error, Result};

/// 抽象凭证仓库。一期实现：[`KeyringStore`]。二期可扩展加密文件后端。
pub trait CredentialStore: Send + Sync {
    /// 保存一个键值对。
    fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()>;

    /// 读取键值。不存在时返回 `Ok(None)`，仅 IO/平台错误时返回 `Err`。
    fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>>;

    /// 删除一个键值。不存在视为成功（幂等）。
    fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()>;
}

const SERVICE: &str = "subswap";

fn compose_key(provider: &str, account: &str, field: &str) -> String {
    format!("{provider}:{account}:{field}")
}

/// 基于系统 keyring 的实现。
/// - macOS: Keychain
/// - Windows: Credential Manager
/// - Linux: secret-service / kernel keyutils
#[derive(Default, Clone)]
pub struct KeyringStore;

impl KeyringStore {
    pub fn new() -> Self {
        Self
    }
}

impl CredentialStore for KeyringStore {
    fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &compose_key(provider, account, field))
            .map_err(|e| Error::Credential(e.to_string()))?;
        entry
            .set_password(value)
            .map_err(|e| Error::Credential(e.to_string()))
    }

    fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(SERVICE, &compose_key(provider, account, field))
            .map_err(|e| Error::Credential(e.to_string()))?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(Error::Credential(e.to_string())),
        }
    }

    fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
        let entry = keyring::Entry::new(SERVICE, &compose_key(provider, account, field))
            .map_err(|e| Error::Credential(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::Credential(e.to_string())),
        }
    }
}
