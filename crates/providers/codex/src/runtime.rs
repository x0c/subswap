//! Codex runtime adapter：差异点接进共享引擎，Codex 私有逻辑（legacy 恢复 / usage 查询）保留在
//! `legacy.rs` / `quota.rs`，本文件只做纯转发，不新增逻辑。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, Quota};
use subswap_provider_common::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};

use crate::codex_files::parse_metadata as parse_auth_metadata;
use crate::paths::{active_auth_path, codex_home};
use crate::{META_AUTH_METADATA, META_CHATGPT_ACCOUNT_ID};

pub const PROVIDER_ID: &str = "codex";

/// Codex runtime adapter：只提供差异点，机制在共享引擎（[`subswap_provider_common::FileBlobProvider`]）。
pub struct CodexRuntime;

#[async_trait]
impl FileBlobRuntime for CodexRuntime {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }
    fn display_name(&self) -> &'static str {
        "Codex / ChatGPT"
    }
    /// 沿用 Codex 历史 keyring/store 字段名，免数据迁移。
    fn store_field(&self) -> &'static str {
        "auth_json"
    }
    fn home(&self) -> PathBuf {
        codex_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        active_auth_path(home)
    }

    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        let m = parse_auth_metadata(blob);
        let mut extra = serde_json::Map::new();
        // 保留供 list 展示与 usage header 使用的字段，与迁移前 registry.toml 布局一致。
        extra.insert(
            META_AUTH_METADATA.into(),
            serde_json::to_value(&m).unwrap_or_default(),
        );
        if let Some(cid) = m.chatgpt_account_id.clone() {
            extra.insert(META_CHATGPT_ACCOUNT_ID.into(), serde_json::Value::String(cid));
        }
        BlobMetadata {
            primary_id: m.primary_id(),
            label: m.label(),
            dedup_key: m.chatgpt_account_id.clone(),
            extra,
        }
    }

    fn isolation(&self) -> IsolationSpec {
        IsolationSpec {
            env_var: "CODEX_HOME",
            native_cli: "codex",
        }
    }

    async fn refresh(&self, _blob: &str) -> Result<RefreshOutcome> {
        // Codex 现状不做带外刷新（query_quota 直接用存量 token；401 由上游 CLI 处理）。
        Ok(RefreshOutcome::Unsupported)
    }

    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        crate::quota::fetch_codex_quota(access_token, account).await
    }

    fn recover_legacy(&self, home: &Path, account: &Account) -> Option<String> {
        crate::legacy::recover_legacy_auth_for_account(home, account)
    }

    fn materialize_extra(&self, _home: &Path, env_dir: &Path) {
        crate::legacy::copy_codex_config_best_effort(env_dir);
    }
}
