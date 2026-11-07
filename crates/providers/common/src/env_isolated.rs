//! 进程环境变量隔离：适用于 API Key 不需要写入私有 Home 的 Provider。
//!
//! `run` / `shell` / `env` 通过该抽象查表获取原生 CLI 与账号环境变量，
//! 不为新的 API Key Provider 增加按名称分发的分支。

use async_trait::async_trait;
use subswap_core::error::Result;
use subswap_core::AccountId;

/// 可直接用进程环境变量隔离账号的 Provider。
#[async_trait]
pub trait EnvIsolatedProvider: Send + Sync {
    /// Provider 标识。
    fn provider_id(&self) -> &'static str;

    /// 该 Provider 隔离启动的原生 CLI。
    fn native_cli(&self) -> &'static str;

    /// 返回账号对应的隔离环境变量；`None` 表示该账号需要走文件物化路径。
    async fn isolation_env(&self, id: &AccountId) -> Result<Option<Vec<(String, String)>>>;
}
