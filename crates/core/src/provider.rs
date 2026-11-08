//! Provider 抽象。每个订阅服务（Codex、Claude、…）实现一个 Provider，
//! 通过 [`crate::registry::ProviderRegistry`] 注册到 CLI / daemon。

use crate::error::Result;
use crate::model::{Account, AccountId, ClientTarget, Quota, QuotaPoolSemantics};
use async_trait::async_trait;

/// 提供给受信本机消费者的当前请求凭证。只允许 access token / API key，
/// 不得把 refresh token 或其他长期轮换秘密放进这个结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestCredential {
    /// 直接用于一次模型请求的 secret。
    pub secret: String,
    /// JWT 等凭证的过期时间（Unix 毫秒）；长期 API key 为 `None`。
    pub expires_at_ms: Option<i64>,
}

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

    /// 投影当前请求所需的短期凭证，供受信本机客户端直接发起模型请求。
    /// 默认不支持；实现不得返回 refresh token，也不得在此方法里切换 active 账号。
    async fn request_credential(&self, _id: &AccountId) -> Result<Option<RequestCredential>> {
        Ok(None)
    }

    /// 按需续期指定账号的请求凭证并返回投影（`subswap auth <id> --renew` 用）。
    /// 默认不续期（返回 `None`，调用方回退到 [`Self::request_credential`] 普通投影）。
    /// 实现必须只处理 `id` 这一个账号：不查额度、不跑 AutoSwap、不改 registry active、
    /// 不拉起 daemon、不返回 refresh token。
    async fn renew_credential(&self, _id: &AccountId) -> Result<Option<RequestCredential>> {
        Ok(None)
    }

    /// 额度池语义（见 [`QuotaPoolSemantics`]）。默认叠加；并行池的 Provider 覆盖。
    /// 共享自动切换逻辑只读该声明，不按 Provider 名分发。
    fn quota_pool_semantics(&self) -> QuotaPoolSemantics {
        QuotaPoolSemantics::Stacked
    }

    /// 切换成功后需要提醒用户的内容（例如官方客户端不热读新号，须重启）。
    /// 无需提醒返回 `None`。
    fn post_swap_notice(&self) -> Option<&'static str> {
        None
    }

    /// 从原生客户端断开登录时是否会退出客户端且不再拉起。
    /// 为 `true` 时 CLI 在断开成功后附一句说明。
    fn disconnect_quits_client(&self) -> bool {
        false
    }

    /// `rm` 清凭证仓库时要删除的字段。默认空（无专属字段）。
    fn credential_store_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// `run`/`shell`/`env` 隔离运行的前置检查。默认直接允许；
    /// 当前官方客户端版本不支持隔离投影时返回错误（不静默起错凭证）。
    async fn ensure_isolation_supported(&self) -> Result<()> {
        Ok(())
    }
}
