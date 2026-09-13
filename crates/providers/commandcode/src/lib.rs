//! Command Code Provider。基于文件型引擎，切换 `~/.commandcode/auth.json` 的 API key。

mod auth;
mod paths;
mod usage;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, AccountRegistry, CredentialStore, Quota};
use subswap_provider_common::{
    BlobMetadata, FileBlobProvider, FileBlobRuntime, IsolationSpec, RefreshOutcome,
};

pub const PROVIDER_ID: &str = "commandcode";

/// Command Code runtime：差异点只在路径、auth 形态与额度查询。
pub struct CommandcodeRuntime;

#[async_trait]
impl FileBlobRuntime for CommandcodeRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "Command Code"
    }
    fn home(&self) -> PathBuf {
        paths::commandcode_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::auth_json_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        auth::parse_metadata(blob)
    }
    fn isolation(&self) -> IsolationSpec {
        // 官方 CLI 用 `homedir()/.commandcode/auth.json`，隔离只能改 HOME。
        IsolationSpec {
            env_var: "HOME",
            native_cli: "command-code",
        }
    }
    async fn refresh(&self, _blob: &str) -> Result<RefreshOutcome> {
        Ok(RefreshOutcome::Unsupported)
    }
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        usage::fetch_quota(access_token, account).await
    }
    fn extract_blob(&self, live_contents: &str) -> Option<String> {
        auth::extract_blob(live_contents)
    }
    fn compose_live(&self, existing_live: Option<&str>, blob: &str) -> String {
        auth::compose_live(existing_live, blob)
    }
    fn access_token(&self, blob: &str) -> Option<String> {
        auth::api_key_from_blob(blob)
    }
    fn isolation_rel_path(&self) -> Option<PathBuf> {
        Some(PathBuf::from(".commandcode").join("auth.json"))
    }
    fn isolation_extra_env(&self, composed_live: &str) -> Vec<(String, String)> {
        let key = auth::api_key_from_live(composed_live).unwrap_or_default();
        vec![("COMMANDCODE_API_KEY".into(), key)]
    }
}

/// 便捷别名：Command Code Provider = 文件型引擎 + Command Code adapter。
pub type CommandcodeProvider = FileBlobProvider<CommandcodeRuntime>;

/// 构造 CommandcodeProvider。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> CommandcodeProvider {
    FileBlobProvider::new(CommandcodeRuntime, store, registry)
}

/// 由粘贴的 API key 生成可导入的 blob。
pub fn blob_from_key(key: &str) -> String {
    auth::blob_from_key(key.trim())
}
