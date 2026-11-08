//! Codex runtime adapter：差异点接进共享引擎，Codex 私有逻辑（legacy 恢复 / usage 查询）保留在
//! `legacy.rs` / `quota.rs`，本文件只做纯转发，不新增逻辑。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, Quota, RequestCredential};
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
    /// `rm` 清凭证仓库时删同一字段。
    fn credential_store_fields(&self) -> &'static [&'static str] {
        &["auth_json"]
    }
    /// 官方客户端不热读新号，已打开的会话须重启。
    fn post_swap_notice(&self) -> Option<&'static str> {
        Some("Restart running Codex CLI sessions to use this account.")
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

    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome> {
        // 停用号：社区同款直连 OAuth + 引擎写回；当前号不走这里（引擎只对 parked 调 refresh）。
        crate::oauth::refresh_parked_blob(blob).await
    }

    async fn renew_active(&self, account: &Account, live_blob: &str) -> Result<RefreshOutcome> {
        // active 续期只走官方 app-server 协调：绝不调直连 OAuth、不查额度。
        let expected = account
            .extra
            .get(META_CHATGPT_ACCOUNT_ID)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                crate::codex_files::parse_metadata(live_blob)
                    .chatgpt_account_id
                    .clone()
            })
            .unwrap_or_default();
        if expected.trim().is_empty() {
            return Err(subswap_core::error::Error::Provider(
                "cannot prove Codex account identity for renewal; refusing active refresh".into(),
            ));
        }
        let home = codex_home();
        let socket = home
            .join("app-server-control")
            .join("app-server-control.sock");
        if tokio::fs::metadata(&socket).await.is_ok() {
            crate::control_socket::refresh_active(&socket, &expected)
                .await
                .map_err(|e| {
                    subswap_core::error::Error::Provider(format!("Codex renewal failed: {e}"))
                })?;
        } else {
            if !crate::app_server::no_codex_process_running_async().await {
                return Err(subswap_core::error::Error::Provider(
                    "Codex process running without a usable control socket; refusing live-home refresh"
                        .into(),
                ));
            }
            crate::app_server::refresh_active_standalone(&expected)
                .await
                .map_err(|e| {
                    subswap_core::error::Error::Provider(format!("Codex renewal failed: {e}"))
                })?;
        }
        let fresh = tokio::fs::read_to_string(active_auth_path(&home))
            .await
            .map_err(|e| {
                subswap_core::error::Error::Provider(format!("read renewed Codex live failed: {e}"))
            })?;
        Ok(RefreshOutcome::Rotated(fresh))
    }

    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>> {
        crate::quota::fetch_codex_quota(access_token, account).await
    }

    fn request_credential(&self, blob: &str) -> Result<Option<RequestCredential>> {
        crate::codex_files::request_credential(blob).map(Some)
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
