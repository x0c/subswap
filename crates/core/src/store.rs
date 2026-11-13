//! 凭证仓库抽象。Token、refresh_token 等敏感字段由 [`CredentialStore`] 持有，
//! 元数据（label、过期时间、优先级等）走 [`crate::paths`] 下的明文 toml/json。
//!
//! 后端：[`KeyringStore`]（系统钥匙串）或 [`FileStore`]（data 目录下 `0600` 明文文件）。
//! macOS 上钥匙串读写会弹授权框，默认装配用 [`FileStore`] 规避（见 cli/daemon 装配处）。
//!
//! 命名约定：
//! - service: `subswap`
//! - key:     `{provider_id}:{account_id}:{field}`，例如 `claude:alice@example.com:access_token`

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs2::FileExt;

use crate::error::{Error, Result};

/// 抽象凭证仓库。实现：[`KeyringStore`]（系统钥匙串）、[`FileStore`]（明文文件）。
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

/// 基于明文 JSON 文件的实现。
///
/// - 单文件存储：`{ "provider:account:field": "value", ... }`。
/// - Unix 下文件权限收紧为 `0600`，仅当前用户可读。
/// - 可挂一个 `legacy` keyring 回退：文件里查不到某项时，从旧钥匙串读出并写回文件，
///   实现 Keychain → 明文文件的「按需、一次性」迁移；迁移后该项永不再碰钥匙串。
///
/// 并发：cli 与 daemon 可能同时读写，读写都经独立锁文件 `<path>.lock` 做 fs2 建议锁，
/// 写入走临时文件 + rename 保证原子性。
pub struct FileStore {
    path: PathBuf,
    legacy: Option<KeyringStore>,
    /// 进程内负缓存：已确认旧钥匙串中不存在（`NoEntry`）或已删除的键，后续不再重复触发系统钥匙串 IPC。
    legacy_misses: Mutex<HashSet<String>>,
}

impl FileStore {
    /// 纯文件实现，不带迁移回退。
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            legacy: None,
            legacy_misses: Mutex::new(HashSet::new()),
        }
    }

    /// 带旧 keyring 回退：文件未命中时从钥匙串取出并落盘迁移。
    pub fn with_legacy_keyring(path: PathBuf, legacy: KeyringStore) -> Self {
        Self {
            path,
            legacy: Some(legacy),
            legacy_misses: Mutex::new(HashSet::new()),
        }
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(suffix);
        PathBuf::from(p)
    }

    /// 打开（必要时创建）锁文件。
    fn open_lock(&self) -> Result<fs::File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Credential(format!("create credentials dir failed: {e}")))?;
        }
        fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.sibling(".lock"))
            .map_err(|e| Error::Credential(format!("open credentials lock failed: {e}")))
    }

    fn read_map(&self) -> Result<BTreeMap<String, String>> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Error::Credential(format!("read credentials failed: {e}"))),
        };
        if raw.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_str(&raw)
            .map_err(|e| Error::Credential(format!("parse credentials failed: {e}")))
    }

    fn write_map(&self, map: &BTreeMap<String, String>) -> Result<()> {
        let raw = serde_json::to_string_pretty(map)
            .map_err(|e| Error::Credential(format!("serialize credentials failed: {e}")))?;
        let tmp = self.sibling(".tmp");
        fs::write(&tmp, raw.as_bytes())
            .map_err(|e| Error::Credential(format!("write credentials failed: {e}")))?;
        restrict_permissions(&tmp)?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| Error::Credential(format!("commit credentials failed: {e}")))
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Credential(format!("chmod credentials failed: {e}")))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

impl CredentialStore for FileStore {
    fn set(&self, provider: &str, account: &str, field: &str, value: &str) -> Result<()> {
        let key = compose_key(provider, account, field);
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .map_err(|e| Error::Credential(format!("lock credentials failed: {e}")))?;
        let result = (|| {
            let mut map = self.read_map()?;
            map.insert(key.clone(), value.to_string());
            self.write_map(&map)
        })();
        let _ = FileExt::unlock(&lock);
        if result.is_ok() {
            if let Ok(mut misses) = self.legacy_misses.lock() {
                misses.remove(&key);
            }
        }
        result
    }

    fn get(&self, provider: &str, account: &str, field: &str) -> Result<Option<String>> {
        let key = compose_key(provider, account, field);
        // 先查文件（共享锁，读完即释放）。
        let hit = {
            let lock = self.open_lock()?;
            FileExt::lock_shared(&lock)
                .map_err(|e| Error::Credential(format!("lock credentials failed: {e}")))?;
            let map = self.read_map();
            let _ = FileExt::unlock(&lock);
            map?.get(&key).cloned()
        };
        if let Some(v) = hit {
            return Ok(Some(v));
        }
        // 文件未命中 → 旧 keyring 回退（best-effort 迁移）；命中即落盘，下次不再读钥匙串。
        // 已确认旧钥匙串不存在的键在进程内负缓存，避免 daemon/CLI 同进程反复探测系统钥匙串。
        // 读失败（钥匙串报错 / 用户拒绝授权）一律当作「没有」，不让迁移源的故障污染查询。
        if let Some(legacy) = &self.legacy {
            let known_miss = self
                .legacy_misses
                .lock()
                .map(|misses| misses.contains(&key))
                .unwrap_or(false);
            if !known_miss {
                match legacy.get(provider, account, field) {
                    Ok(Some(v)) => {
                        self.set(provider, account, field, &v)?;
                        return Ok(Some(v));
                    }
                    Ok(None) => {
                        if let Ok(mut misses) = self.legacy_misses.lock() {
                            misses.insert(key);
                        }
                    }
                    Err(e) => {
                        tracing::debug!(err = %e, "legacy keyring fallback failed; treating as missing");
                    }
                }
            }
        }
        Ok(None)
    }

    fn delete(&self, provider: &str, account: &str, field: &str) -> Result<()> {
        let key = compose_key(provider, account, field);
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)
            .map_err(|e| Error::Credential(format!("lock credentials failed: {e}")))?;
        let result = (|| {
            let mut map = self.read_map()?;
            map.remove(&key);
            self.write_map(&map)
        })();
        let _ = FileExt::unlock(&lock);
        // 同步清掉钥匙串旧副本，否则下次 get 会把已删项又迁回来；已确认无旧副本时跳过 IPC。
        if let Some(legacy) = &self.legacy {
            let known_miss = self
                .legacy_misses
                .lock()
                .map(|misses| misses.contains(&key))
                .unwrap_or(false);
            if !known_miss {
                let _ = legacy.delete(provider, account, field);
                if let Ok(mut misses) = self.legacy_misses.lock() {
                    misses.insert(key);
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("credentials.json"));
        (dir, store)
    }

    #[test]
    fn set_get_delete_roundtrip() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.get("claude", "a@x.com", "credentials_json").unwrap(),
            None
        );

        store
            .set("claude", "a@x.com", "credentials_json", "{\"t\":1}")
            .unwrap();
        assert_eq!(
            store.get("claude", "a@x.com", "credentials_json").unwrap(),
            Some("{\"t\":1}".to_string())
        );

        // 覆盖写。
        store
            .set("claude", "a@x.com", "credentials_json", "{\"t\":2}")
            .unwrap();
        assert_eq!(
            store.get("claude", "a@x.com", "credentials_json").unwrap(),
            Some("{\"t\":2}".to_string())
        );

        store
            .delete("claude", "a@x.com", "credentials_json")
            .unwrap();
        assert_eq!(
            store.get("claude", "a@x.com", "credentials_json").unwrap(),
            None
        );
        // 删不存在的项幂等。
        store
            .delete("claude", "a@x.com", "credentials_json")
            .unwrap();
    }

    #[test]
    fn keys_are_namespaced_by_provider_account_field() {
        let (_dir, store) = temp_store();
        store.set("claude", "a@x.com", "f", "claude-val").unwrap();
        store.set("codex", "a@x.com", "f", "codex-val").unwrap();
        store.set("claude", "b@x.com", "f", "other-acct").unwrap();

        assert_eq!(
            store.get("claude", "a@x.com", "f").unwrap(),
            Some("claude-val".to_string())
        );
        assert_eq!(
            store.get("codex", "a@x.com", "f").unwrap(),
            Some("codex-val".to_string())
        );
        assert_eq!(
            store.get("claude", "b@x.com", "f").unwrap(),
            Some("other-acct".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = temp_store();
        store.set("claude", "a@x.com", "f", "secret").unwrap();
        let mode = std::fs::metadata(&store.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
    }

    #[test]
    fn legacy_miss_cache_clears_on_set_and_tracks_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            FileStore::with_legacy_keyring(dir.path().join("credentials.json"), KeyringStore::new());
        let key = compose_key("test-no-legacy", "nobody-63c3c24d", "missing_field");
        store.legacy_misses.lock().unwrap().insert(key.clone());
        // 负缓存命中时直接返回 None，不触碰系统钥匙串。
        assert_eq!(
            store
                .get("test-no-legacy", "nobody-63c3c24d", "missing_field")
                .unwrap(),
            None
        );
        // set 写入文件后清除负缓存标记。
        store
            .set("test-no-legacy", "nobody-63c3c24d", "missing_field", "val")
            .unwrap();
        assert!(!store.legacy_misses.lock().unwrap().contains(&key));
        assert_eq!(
            store
                .get("test-no-legacy", "nobody-63c3c24d", "missing_field")
                .unwrap(),
            Some("val".to_string())
        );
        // 预置负缓存后 delete 同样跳过系统钥匙串 IPC，并从文件删除。
        store.legacy_misses.lock().unwrap().insert(key.clone());
        store
            .delete("test-no-legacy", "nobody-63c3c24d", "missing_field")
            .unwrap();
        assert_eq!(
            store
                .get("test-no-legacy", "nobody-63c3c24d", "missing_field")
                .unwrap(),
            None
        );
    }
}
