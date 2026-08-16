//! OpenCode Go 订阅 Provider。基于文件型引擎，只切换 `auth.json` 的 `opencode-go` 条目。

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

pub const PROVIDER_ID: &str = "opencode";

/// OpenCode Go runtime：差异点只在路径、局部合并、API key 与额度查询。
pub struct OpencodeRuntime;

#[async_trait]
impl FileBlobRuntime for OpencodeRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "OpenCode Go"
    }
    fn home(&self) -> PathBuf {
        paths::opencode_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        paths::auth_json_path(home)
    }
    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        auth::parse_metadata(blob)
    }
    fn isolation(&self) -> IsolationSpec {
        IsolationSpec {
            env_var: "XDG_DATA_HOME",
            native_cli: "opencode",
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
        Some(PathBuf::from("opencode").join("auth.json"))
    }
    fn isolation_extra_env(&self, composed_live: &str) -> Vec<(String, String)> {
        vec![("OPENCODE_AUTH_CONTENT".into(), composed_live.to_string())]
    }
}

/// 便捷别名：OpenCode Provider = 文件型引擎 + OpenCode adapter。
pub type OpencodeProvider = FileBlobProvider<OpencodeRuntime>;

/// 构造 OpenCodeProvider。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> OpencodeProvider {
    FileBlobProvider::new(OpencodeRuntime, store, registry)
}

/// 由粘贴的 API key 生成可导入的 blob。
pub fn blob_from_key(key: &str) -> String {
    auth::blob_from_key(key.trim())
}
