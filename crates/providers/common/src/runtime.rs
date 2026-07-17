//! 文件型 provider 的 adapter 契约：每个 runtime 只实现差异点，机制在 [`crate::engine`]。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::{Account, Quota};

/// 从凭证 blob 解析出的最小元数据。所有可选字段最终可能进 registry.toml，
/// 注意：写进 `extra` 的 `Option` 值不要保留 null（引擎只写 `Some`）。
#[derive(Debug, Clone, Default)]
pub struct BlobMetadata {
    /// account 主键候选（如 Codex account_key / Kimi user_id）。
    pub primary_id: Option<String>,
    /// 展示 label（如 email / user_id）。
    pub label: Option<String>,
    /// 跨主键去重用的稳定键（如 Codex chatgpt_account_id）；无则 None。
    pub dedup_key: Option<String>,
    /// 额外落进 registry.toml `extra` 的字段（provider 私有，如会员档/额度用 header）。
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 隔离运行所需的差异点。
#[derive(Debug, Clone)]
pub struct IsolationSpec {
    /// 隔离环境变量名（Codex `CODEX_HOME` / Kimi `KIMI_CODE_HOME`）。
    pub env_var: &'static str,
    /// 原生 CLI 可执行名（`codex` / `kimi`）。
    pub native_cli: &'static str,
}

/// 刷新结果。
pub enum RefreshOutcome {
    /// 刷新成功，返回轮换后的完整 blob。
    Rotated(String),
    /// refresh token 已失效（invalid_grant / 401 / 403），需重新登录。
    DeadToken,
    /// 该 runtime 不支持刷新（如纯 API key）。
    Unsupported,
}

/// 每个文件型 runtime 的差异点契约。机制（切换/回滚/回灌/隔离）在引擎里，不在这里。
#[async_trait]
pub trait FileBlobRuntime: Send + Sync + 'static {
    /// provider 标识，如 "codex" / "kimi"。
    fn id(&self) -> &'static str;
    /// 人类可读名称。
    fn display_name(&self) -> &'static str;
    /// store 里存 blob 的字段名。默认 "blob"；Codex 为兼容历史数据返回 "auth_json"。
    fn store_field(&self) -> &'static str {
        "blob"
    }
    /// 解析 provider 工作目录（读 env + 默认目录）。
    fn home(&self) -> PathBuf;
    /// 工作目录内的 live 凭证文件路径。
    fn live_cred_path(&self, home: &Path) -> PathBuf;
    /// 从 blob 抽最小元数据。解析失败返回 `Default`（透传策略，不 panic）。
    fn parse_metadata(&self, blob: &str) -> BlobMetadata;
    /// 隔离运行差异点。
    fn isolation(&self) -> IsolationSpec;

    /// 刷新一个 blob，返回轮换后的完整 blob。仅对 parked 账号调用。
    async fn refresh(&self, blob: &str) -> Result<RefreshOutcome>;
    /// 查询额度。`access_token` 由引擎保证是新鲜的（parked 已按需刷新）。
    async fn fetch_quota(&self, access_token: &str, account: &Account) -> Result<Vec<Quota>>;

    /// 可选：在 store/live 都拿不到时，从 provider 私有 legacy 布局恢复 blob。默认无。
    fn recover_legacy(&self, _home: &Path, _account: &Account) -> Option<String> {
        None
    }

    /// 可选：额外物化（如复制真实 config 进隔离目录）。默认无。
    fn materialize_extra(&self, _home: &Path, _env_dir: &Path) {}
}
