//! 用户显式删除过的账号墓碑。
//!
//! 默认入口会把各客户端当前登录账号自动 import 进 registry。若用户刚 `subswap rm`
//! 掉的正好是那个正在登录的号，下次无参 `subswap` 会把它当「没记录过」再导入回来，
//! 删除看起来没生效。墓碑让自动导入跳过这些 id，直到用户再跑一次 `subswap login`。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Default, Serialize, Deserialize)]
struct RemovedFile {
    /// provider → 已删除的账号 id。
    #[serde(default)]
    accounts: HashMap<String, Vec<String>>,
}

/// 显式 `rm` 留下的账号墓碑，阻止默认入口把当前登录账号再自动加回来。
#[derive(Debug, Clone)]
pub struct RemovedAccounts {
    path: PathBuf,
    by_provider: HashMap<String, HashSet<String>>,
}

impl RemovedAccounts {
    /// 从文件加载；文件不存在或解析失败视为没有墓碑。
    pub fn load(path: &Path) -> Self {
        let by_provider = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<RemovedFile>(&raw).ok())
            .map(|file| {
                file.accounts
                    .into_iter()
                    .map(|(provider, ids)| (provider, ids.into_iter().collect()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            by_provider,
        }
    }

    /// 该账号是否被用户显式删除过、且尚未被 `login` 解除。
    pub fn contains(&self, provider: &str, id: &str) -> bool {
        self.by_provider
            .get(provider)
            .is_some_and(|ids| ids.contains(id))
    }

    /// 记下一次显式删除。已存在时幂等。
    pub fn add(&mut self, provider: &str, id: &str) -> Result<()> {
        self.by_provider
            .entry(provider.to_string())
            .or_default()
            .insert(id.to_string());
        self.save()
    }

    /// `subswap login` 成功导入后解除墓碑，允许该账号重新出现在列表里。
    pub fn clear(&mut self, provider: &str, id: &str) -> Result<()> {
        let mut changed = false;
        if let Some(ids) = self.by_provider.get_mut(provider) {
            changed = ids.remove(id);
            if ids.is_empty() {
                self.by_provider.remove(provider);
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = RemovedFile {
            accounts: self
                .by_provider
                .iter()
                .map(|(provider, ids)| {
                    let mut ids: Vec<String> = ids.iter().cloned().collect();
                    ids.sort();
                    (provider.clone(), ids)
                })
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_contains_and_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed.json");
        let mut removed = RemovedAccounts::load(&path);
        assert!(!removed.contains("cursor", "auth0|user_a"));

        removed.add("cursor", "auth0|user_a").unwrap();
        assert!(removed.contains("cursor", "auth0|user_a"));
        assert!(!removed.contains("cursor", "auth0|user_b"));

        let reloaded = RemovedAccounts::load(&path);
        assert!(reloaded.contains("cursor", "auth0|user_a"));

        let mut removed = reloaded;
        removed.clear("cursor", "auth0|user_a").unwrap();
        assert!(!removed.contains("cursor", "auth0|user_a"));
        let reloaded = RemovedAccounts::load(&path);
        assert!(!reloaded.contains("cursor", "auth0|user_a"));
    }
}
