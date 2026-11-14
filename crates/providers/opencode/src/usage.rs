//! OpenCode Go 额度：GET `{base}/usage`，Bearer API key。
//! 共用实现位于 [`subswap_provider_common::opencode_go_usage`]。

use subswap_core::error::Result;
use subswap_core::{Account, Quota};

pub use subswap_provider_common::opencode_go_usage::parse_usage;

/// 用 Go API key 查额度。
pub async fn fetch_quota(access_token: &str, account: &Account) -> Result<Vec<Quota>> {
    subswap_provider_common::opencode_go_usage::fetch_quota(
        access_token,
        &account.provider,
        &account.id,
    )
    .await
}
