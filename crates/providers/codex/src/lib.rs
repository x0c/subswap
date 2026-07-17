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
        _meta: serde_json::Value,
        active: bool,
    ) -> Result<subswap_core::Account> {
        self.import_raw(raw, None, Some(active))
    }
}
