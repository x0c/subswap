//! Codex / ChatGPT Provider：基于 `subswap-provider-common` 的文件型共享引擎组装。
//!
//! 关键约束（机制在 [`subswap_provider_common::engine`]，此处只留 Codex 差异点）：
//! - `activate` **不依赖** `query_quota`：网络不通也能切换。
//! - 整段 `~/.codex/auth.json` 作为 opaque blob 进 store，subswap 不假设 schema 稳定。
//! - 切换 = flock → snapshot 旧文件 → 原子写新 auth.json → 任一步失败回滚。

mod codex_files;
mod legacy;
mod openai_usage;
mod paths;
mod quota;
mod runtime;

use std::sync::Arc;

use subswap_core::error::Result;
use subswap_core::{AccountId, AccountRegistry, CredentialStore};
use subswap_provider_common::FileBlobProvider;

pub use runtime::{CodexRuntime, PROVIDER_ID};

/// registry.toml `extra.chatgpt_account_id` 字段名，用于额度查询时拼 header，
/// 也是跨主键去重的历史键名（沿用旧数据，不做迁移）。
pub(crate) const META_CHATGPT_ACCOUNT_ID: &str = "chatgpt_account_id";
/// registry.toml `extra.auth_metadata` 字段名，存 [`codex_files::AuthMetadata`] 全量供 list 展示。
pub(crate) const META_AUTH_METADATA: &str = "auth_metadata";

/// 便捷别名：Codex Provider = 文件型共享引擎 + Codex adapter。
pub type CodexProvider = FileBlobProvider<CodexRuntime>;

/// 构造 CodexProvider（与 KimiProvider/ClaudeProvider 的 `::new` 调用面一致）。
pub fn new(store: Arc<dyn CredentialStore>, registry: Arc<AccountRegistry>) -> CodexProvider {
    FileBlobProvider::new(CodexRuntime, store, registry)
}

/// 兼容旧调用名：CLI/daemon 现有代码调用这些方法名。内部转发到共享引擎的通用方法，
/// 不新增逻辑，只是为了不必大改调用点（`run.rs`/`migrate.rs`）。
pub trait CodexCompat {
    /// 导出账号 auth.json 原文，供 `subswap run` 写入隔离环境（`CODEX_HOME`）。
    fn export_auth_blob(&self, id: &AccountId) -> Result<String>;
    /// 隔离会话结束后吸收（可能被 codex CLI 轮换过的）auth.json，仅更新该账号的 store 副本。
    fn absorb_auth_blob(&self, id: &AccountId, raw: &str) -> Result<()>;
    /// 从旧版 registry 元数据 + opaque auth blob 导入账号（`subswap migrate` 用）。
    fn import_raw_with_metadata(
        &self,
        raw: String,
        meta: serde_json::Value,
        active: bool,
    ) -> Result<subswap_core::Account>;
}

impl CodexCompat for CodexProvider {
    fn export_auth_blob(&self, id: &AccountId) -> Result<String> {
        self.export_blob(id)
    }
    fn absorb_auth_blob(&self, id: &AccountId, raw: &str) -> Result<()> {
        self.absorb_blob(id, raw)
    }
    fn import_raw_with_metadata(
        &self,
        raw: String,
        meta: serde_json::Value,
        active: bool,
    ) -> Result<subswap_core::Account> {
        // 优先用调用方提供的 legacy metadata（可能带有当前 blob 解析逻辑推导不出的字段，
        // 如缓存的 last_usage/last_usage_at）；只有反序列化失败（旧 schema 变化）时才退回
        // 从 raw blob 重新派生，与迁移前 `store_account_with_metadata` 的行为保持一致。
        match serde_json::from_value::<codex_files::AuthMetadata>(meta) {
            Ok(auth_meta) => {
                let blob_metadata = runtime::auth_metadata_to_blob_metadata(auth_meta);
                self.import_raw_with_explicit_metadata(raw, blob_metadata, Some(active))
            }
            Err(_) => self.import_raw(raw, None, Some(active)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subswap_core::{AccountRegistry, FileStore};

    fn test_provider(tmp: &std::path::Path) -> CodexProvider {
        let store: Arc<dyn CredentialStore> = Arc::new(FileStore::new(tmp.join("creds.json")));
        let registry = Arc::new(AccountRegistry::new(tmp.join("registry.toml")));
        new(store, registry)
    }

    /// Finding 2 回归：`import_raw_with_metadata`（`subswap migrate` 的一次性导入路径）必须
    /// 优先用调用方传入的 legacy metadata，而不是无脑重新从 raw blob 派生——否则 legacy
    /// registry 里带有、但当前 blob 解析不出的字段（这里用 last_usage 缓存举例）会在迁移时静默丢失。
    ///
    /// 修复前：`import_raw_with_metadata` 直接忽略 `meta` 参数、总是调用
    /// `self.import_raw(raw, None, Some(active))` 重新从 raw 派生，last_usage/last_usage_at 丢失。
    #[test]
    fn import_raw_with_metadata_prefers_caller_supplied_metadata_over_raw_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let p = test_provider(tmp.path());

        // raw blob 只有 account_key + token，从它派生的 metadata 不会带 last_usage 缓存。
        let raw = r#"{"account_key":"key-abc","tokens":{"access_token":"AT"}}"#.to_string();

        // legacy registry 条目里的 metadata：带 raw 派生不出的 last_usage 缓存字段。
        let legacy_meta = serde_json::json!({
            "account_key": "key-abc",
            "email": "user@example.com",
            "last_usage": {"primary": {"used_percent": 42}},
            "last_usage_at": 1_700_000_000i64,
        });

        let account = p.import_raw_with_metadata(raw, legacy_meta, true).unwrap();
        assert_eq!(account.id.0, "key-abc");
        assert_eq!(account.label, "user@example.com");

        let auth_metadata = account
            .extra
            .get(META_AUTH_METADATA)
            .expect("auth_metadata 字段应存在");
        assert_eq!(
            auth_metadata.get("last_usage_at").and_then(|v| v.as_i64()),
            Some(1_700_000_000),
            "legacy metadata 的 last_usage 缓存字段应保留，不应因为 raw 派生不出而丢失"
        );
        assert_eq!(
            auth_metadata
                .get("last_usage")
                .and_then(|v| v.get("primary"))
                .and_then(|v| v.get("used_percent"))
                .and_then(|v| v.as_i64()),
            Some(42),
        );
    }

    /// meta 反序列化失败（如旧 schema 变化、字段类型不兼容）时应退回从 raw blob 派生，
    /// 而不是直接报错——与迁移前 `unwrap_or_else(|_| parse_metadata(&raw_auth_json))` 的
    /// 兜底行为一致。
    #[test]
    fn import_raw_with_metadata_falls_back_to_raw_derivation_when_metadata_json_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let p = test_provider(tmp.path());
        let raw = r#"{"account_key":"key-xyz","email":"fallback@example.com"}"#.to_string();

        // meta 不是合法的 AuthMetadata 形状（这里给一个数组），应回退到从 raw 派生。
        let account = p
            .import_raw_with_metadata(raw, serde_json::json!([1, 2, 3]), true)
            .unwrap();
        assert_eq!(account.id.0, "key-xyz");
        assert_eq!(account.label, "fallback@example.com");
    }
}
