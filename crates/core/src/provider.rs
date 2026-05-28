//! Provider 抽象。每个订阅服务（Codex、Claude、…）实现一个 Provider，
//! 通过 [`crate::registry::ProviderRegistry`] 注册到 CLI / daemon。

use crate::error::Result;
use crate::model::{Account, AccountId, ClientTarget, Quota};
use async_trait::async_trait;

/// Provider 接口。
///
/// 设计要点：
/// - 所有可能阻塞的方法都是 async，统一在 tokio 上调度。
/// - 凭证读写不直接暴露 token；Provider 持有 [`crate::store::CredentialStore`] 引用。
/// - `activate` 必须保证多客户端的原子性（失败回滚），由实现内部加文件锁。
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider 标识，例如 "codex" / "claude"。CLI 命令里会用到。
    fn id(&self) -> &'static str;

    /// 人类可读名称。
    fn display_name(&self) -> &'static str;

    /// 该 Provider 涉及的本地客户端目标。doctor 命令用它探测是否安装。
    fn client_targets(&self) -> Vec<ClientTarget>;

    /// 列出该 Provider 下所有已配置的账号。
    async fn list_accounts(&self) -> Result<Vec<Account>>;

    /// 把指定账号切为激活态，并同步所有 `client_targets` 的本地文件。
    async fn activate(&self, id: &AccountId) -> Result<()>;

    /// 查询某账号的额度。可能返回多窗口（例如 Claude 的 5h + 7d）。
    /// 实现允许返回 `Vec` 为空表示"暂无可查"，但应优先返回 status=Unknown 的占位。
    async fn query_quota(&self, id: &AccountId) -> Result<Vec<Quota>>;
}
