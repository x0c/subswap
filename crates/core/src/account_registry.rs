//! 账号元数据注册表：`<config_dir>/registry.toml` 的读写封装。
//!
//! 文件结构（示例）：
//! ```toml
//! [[accounts]]
//! provider = "claude"
//! id = "alice@example.com"
//! label = "alice"
//! active = true
//! created_at = "2026-05-28T10:00:00Z"
//! priority = 100
//!
//! [accounts.extra]
//! email = "alice@example.com"
//! account_uuid = "..."
//! organization_uuid = "..."
//! organization_name = "Personal"
//! ```
//!
//! token / refresh_token 这类敏感字段**不在这里**，由 [`crate::CredentialStore`] 持有。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{Account, AccountId};
use crate::paths::AppPaths;

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    accounts: Vec<Account>,
}

/// 账号元数据注册表。
///
/// 设计上故意做成「每次加载/保存都全量读写」：
/// - 数据量小（账号数量个位数）。
/// - 避免内存中长期持有可能过期的快照。
pub struct AccountRegistry {
    path: PathBuf,
}

impl AccountRegistry {
    /// 用默认 [`AppPaths`] 解析路径。
    pub fn from_default_paths() -> Result<Self> {
        let paths = AppPaths::resolve()?;
        Ok(Self::new(paths.registry_file()))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// 加载注册表；文件不存在时返回空列表。
    pub fn load(&self) -> Result<Vec<Account>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }
        let parsed: RegistryFile = toml::from_str(&content)?;
        Ok(parsed.accounts)
    }

    /// 保存（全量覆写）。先写 tmp 再 rename，避免半截文件。
    pub fn save(&self, accounts: &[Account]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut accounts = accounts.to_vec();
        for account in &mut accounts {
            account.extra.retain(|_, value| strip_json_nulls(value));
        }
        let serialized = toml::to_string_pretty(&RegistryFile { accounts })?;
        let tmp = self
            .path
            .with_extension(format!("toml.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serialized)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// 列出某个 provider 下的账号。
    pub fn list_by_provider(&self, provider: &str) -> Result<Vec<Account>> {
        Ok(self
            .load()?
            .into_iter()
            .filter(|a| a.provider == provider)
            .collect())
    }

    /// 查询某个账号。
    pub fn find(&self, provider: &str, id: &AccountId) -> Result<Option<Account>> {
        Ok(self
            .load()?
            .into_iter()
            .find(|a| a.provider == provider && a.id == *id))
    }

    /// 在某个 provider 下按 id 或 label 查询唯一账号。
    pub fn find_unique_in_provider(
        &self,
        provider: &str,
        id_or_label: &str,
    ) -> Result<Option<Account>> {
        let matches: Vec<Account> = self
            .load()?
            .into_iter()
            .filter(|a| a.provider == provider && account_matches(a, id_or_label))
            .collect();
        unique_match(matches, id_or_label)
    }

    /// 跨 provider 按 id 反查唯一账号。
    ///
    /// 三种返回：
    /// - `Ok(Some(a))`：唯一命中。
    /// - `Ok(None)`：无任何账号匹配该 id。
    /// - `Err(Error::Other(_))`：多于一个 provider 持有同 id（应让用户用 `<provider>/<id>` 显式定位）。
    ///
    /// 也支持 `<provider>/<id>` 显式前缀形式：例如 `claude/alice@x` 直接锁定 provider，
    /// 绕过歧义检测。
    pub fn find_unique(&self, id_or_qualified: &str) -> Result<Option<Account>> {
        // 显式 provider/id 前缀。
        if let Some((provider, id)) = id_or_qualified.split_once('/') {
            return self.find_unique_in_provider(provider, id);
        }
        let matches: Vec<Account> = self
            .load()?
            .into_iter()
            .filter(|a| account_matches(a, id_or_qualified))
            .collect();
        unique_match(matches, id_or_qualified)
    }

    /// 插入或更新一个账号（按 (provider, id) 主键去重）。
    pub fn upsert(&self, account: Account) -> Result<()> {
        let mut all = self.load()?;
        if let Some(existing) = all
            .iter_mut()
            .find(|a| a.provider == account.provider && a.id == account.id)
        {
            *existing = account;
        } else {
            all.push(account);
        }
        self.save(&all)
    }

    /// 删除一个账号。
    pub fn remove(&self, provider: &str, id: &AccountId) -> Result<()> {
        let mut all = self.load()?;
        let before = all.len();
        all.retain(|a| !(a.provider == provider && a.id == *id));
        if all.len() == before {
            return Err(Error::AccountNotFound {
                provider: provider.into(),
                id: id.to_string(),
            });
        }
        self.save(&all)
    }

    /// 把指定账号标记为该 Provider 的激活账号，其他同 Provider 账号置为 inactive。
    pub fn set_active(&self, provider: &str, id: &AccountId) -> Result<()> {
        let mut all = self.load()?;
        let mut found = false;
        for a in &mut all {
            if a.provider == provider {
                let is_target = a.id == *id;
                a.active = is_target;
                if is_target {
                    found = true;
                    a.last_used_at = Some(chrono::Utc::now());
                }
            }
        }
        if !found {
            return Err(Error::AccountNotFound {
                provider: provider.into(),
                id: id.to_string(),
            });
        }
        self.save(&all)
    }
}

fn account_matches(account: &Account, id_or_label: &str) -> bool {
    account.id.0 == id_or_label || account.label == id_or_label
}

fn unique_match(mut matches: Vec<Account>, id_or_label: &str) -> Result<Option<Account>> {
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => {
            let providers: Vec<String> = matches.iter().map(|a| a.provider.clone()).collect();
            Err(Error::Other(anyhow::anyhow!(
                "ambiguous account {:?}: found in providers {:?}; \
                 disambiguate with `<provider>/<id-or-label>`",
                id_or_label,
                providers
            )))
        }
    }
}

/// TOML 没有 null 类型；写 registry 前把 Provider 私有 metadata 里的 JSON null 清掉。
fn strip_json_nulls(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Object(map) => {
            map.retain(|_, child| strip_json_nulls(child));
            true
        }
        serde_json::Value::Array(items) => {
            items.retain_mut(strip_json_nulls);
            true
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AccountId;

    fn make_account(provider: &str, id: &str) -> Account {
        Account {
            provider: provider.into(),
            id: AccountId(id.into()),
            label: id.into(),
            active: false,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    fn make_labeled_account(provider: &str, id: &str, label: &str) -> Account {
        let mut account = make_account(provider, id);
        account.label = label.into();
        account
    }

    #[test]
    fn upsert_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        reg.upsert(make_account("claude", "a")).unwrap();
        reg.upsert(make_account("claude", "b")).unwrap();
        reg.upsert(make_account("codex", "c")).unwrap();

        let all = reg.load().unwrap();
        assert_eq!(all.len(), 3);

        let claudes = reg.list_by_provider("claude").unwrap();
        assert_eq!(claudes.len(), 2);
    }

    #[test]
    fn save_strips_json_nulls_from_extra() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        let mut account = make_account("claude", "a");
        account.extra.insert(
            "metadata".into(),
            serde_json::json!({
                "keep": "value",
                "drop": null,
                "nested": { "drop": null, "keep": 1 },
                "list": [1, null, 2]
            }),
        );

        reg.upsert(account).unwrap();
        let loaded = reg.load().unwrap();
        let metadata = loaded[0].extra.get("metadata").unwrap();
        assert_eq!(metadata["keep"], "value");
        assert!(metadata.get("drop").is_none());
        assert!(metadata["nested"].get("drop").is_none());
        assert_eq!(metadata["list"], serde_json::json!([1, 2]));
    }

    #[test]
    fn find_unique_returns_some_when_single_match() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        reg.upsert(make_account("claude", "alice")).unwrap();
        reg.upsert(make_account("codex", "bob")).unwrap();

        let a = reg.find_unique("alice").unwrap().unwrap();
        assert_eq!(a.provider, "claude");
    }

    #[test]
    fn find_unique_matches_label() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        reg.upsert(make_labeled_account(
            "codex",
            "user-long-key",
            "alice@example.com",
        ))
        .unwrap();

        let a = reg.find_unique("alice@example.com").unwrap().unwrap();
        assert_eq!(a.id.0, "user-long-key");

        let qualified = reg.find_unique("codex/alice@example.com").unwrap().unwrap();
        assert_eq!(qualified.id.0, "user-long-key");
    }

    #[test]
    fn find_unique_returns_err_when_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        reg.upsert(make_account("claude", "alice")).unwrap();
        reg.upsert(make_account("codex", "alice")).unwrap();

        let err = reg.find_unique("alice").unwrap_err().to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn find_unique_qualified_prefix_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        reg.upsert(make_account("claude", "alice")).unwrap();
        reg.upsert(make_account("codex", "alice")).unwrap();

        let a = reg.find_unique("codex/alice").unwrap().unwrap();
        assert_eq!(a.provider, "codex");
    }

    #[test]
    fn set_active_marks_exactly_one() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AccountRegistry::new(tmp.path().join("registry.toml"));
        reg.upsert(make_account("claude", "a")).unwrap();
        reg.upsert(make_account("claude", "b")).unwrap();
        reg.set_active("claude", &AccountId("b".into())).unwrap();

        let all = reg.load().unwrap();
        let actives: Vec<_> = all.iter().filter(|a| a.active).collect();
        assert_eq!(actives.len(), 1);
        assert_eq!(actives[0].id.0, "b");
    }
}
