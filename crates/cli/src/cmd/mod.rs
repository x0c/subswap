//! 各子命令实现。每个子命令独立成模块，便于单文件聚焦阅读。

pub mod default;
pub mod doctor;
pub mod login;
pub mod migrate;
pub mod rm;
pub mod swap;

use anyhow::{Context, Result};
use subswap_core::Account;

use crate::app::AppContext;

/// 把用户传入的引用解析到具体账号。两种形式：
/// - **纯数字 N**（≥1）：取 [`AppContext::list_ordered`] 的第 N 个。`subswap` 默认入口、`swap`、`rm`
///   共用同一编号映射，所以「屏幕上看到的第 3 行」就是 `swap 3` 切的那个。
/// - **id / label / `provider/id`**：走 [`subswap_core::AccountRegistry::find_unique`]，与原行为兼容。
///
/// 设计上保留 `find_unique` 路径，让脚本里 `subswap swap alice@x.com` 的写法继续可用，
/// 不为了短编号牺牲已存在的稳定 API。
pub fn resolve_account(ctx: &AppContext, input: &str) -> Result<Account> {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        if n == 0 {
            anyhow::bail!("invalid account index 0; numbering starts at 1");
        }
        let ordered = ctx.list_ordered()?;
        return ordered
            .into_iter()
            .nth(n - 1)
            .with_context(|| format!("no account at index {n}; run `subswap` to see the list"));
    }
    ctx.registry
        .find_unique(trimmed)?
        .with_context(|| format!("account not found: {trimmed}"))
}
