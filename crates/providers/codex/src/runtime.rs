//! Codex runtime adapter：差异点接进共享引擎，Codex 私有逻辑（legacy 恢复 / usage 查询）保留在
//! `legacy.rs` / `quota.rs`，本文件只做纯转发，不新增逻辑。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, Quota};
use subswap_provider_common::{BlobMetadata, FileBlobRuntime, IsolationSpec, RefreshOutcome};

use crate::codex_files::{parse_metadata as parse_auth_metadata, AuthMetadata};
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
    /// 沿用此次迁移前 `registry.toml` 就已存在的键名，兼容存量账号数据（无需迁移即可继续匹配）。
    fn dedup_extra_key(&self) -> &'static str {
        META_CHATGPT_ACCOUNT_ID
    }
    fn home(&self) -> PathBuf {
        codex_home()
    }
    fn live_cred_path(&self, home: &Path) -> PathBuf {
        active_auth_path(home)
    }

    fn parse_metadata(&self, blob: &str) -> BlobMetadata {
        auth_metadata_to_blob_metadata(parse_auth_metadata(blob))
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

/// 把 [`AuthMetadata`] 转成引擎通用的 [`BlobMetadata`]。供 `parse_metadata`（从 blob 派生）和
/// `CodexCompat::import_raw_with_metadata`（legacy 迁移场景，metadata 由调用方提供、不重新派生）
/// 共用，避免两处转换逻辑各写一份、慢慢分叉。
///
/// 注意：`extra` 只塞 [`META_AUTH_METADATA`]；`dedup_key` 对应的 `extra[dedup_extra_key()]`
/// （即 [`META_CHATGPT_ACCOUNT_ID`]）由引擎 `store_account` 统一从 `dedup_key` 字段写入，
/// 这里不重复插入，否则会有两处来源都往同一个键写值。
pub(crate) fn auth_metadata_to_blob_metadata(m: AuthMetadata) -> BlobMetadata {
    let mut extra = serde_json::Map::new();
    extra.insert(
        META_AUTH_METADATA.into(),
        serde_json::to_value(&m).unwrap_or_default(),
    );
    BlobMetadata {
        primary_id: m.primary_id(),
        label: m.label(),
        dedup_key: m.chatgpt_account_id.clone(),
        extra,
    }
}
