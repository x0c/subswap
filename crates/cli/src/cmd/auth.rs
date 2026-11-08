//! 受信本机消费者的请求凭证投影。输出永远含 secret，调用方必须当作敏感 IPC 处理。

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde::Serialize;

use super::resolve_account;
use crate::app::AppContext;

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AuthProjection {
    Credential {
        secret: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_at_ms: Option<i64>,
    },
    Environment {
        env: BTreeMap<String, String>,
    },
}

#[derive(Serialize)]
struct AuthOutput {
    schema_version: u8,
    provider: String,
    id: String,
    #[serde(flatten)]
    projection: AuthProjection,
}

/// 把一个账号的当前请求凭证以单个 JSON 对象写到 stdout。
/// 不切换 active、不查额度、不刷新 token；需要续期时由消费者走 Subswap 既有协调路径后重读。
/// `renew=true`（`--renew`）时：先按 60 秒续期窗判断，仍新鲜则直接投影；
/// 临近过期才只续期该账号（parked 走既有 OAuth，active 走官方 app-server 协调），
/// 再返回同一单 JSON 投影；绝不碰额度/默认入口/AutoSwap/active 切换/拉起 subswapd；官方 codex app-server 子进程协调除外。
pub async fn run(ctx: &AppContext, input: &str, renew: bool) -> Result<()> {
    let account = resolve_account(ctx, input)?;
    let provider = ctx.providers.get(&account.provider)?;

    // `--renew`：先走 provider 续期钩子；返回 `None` 表示该 provider 无续期实现，
    // 回退到普通只读投影（长效 key/环境投影直接返回，不做网络/子进程工作）。
    let renewed = if renew {
        provider.renew_credential(&account.id).await?
    } else {
        None
    };
    let projection = if let Some(credential) = renewed {
        AuthProjection::Credential {
            secret: credential.secret,
            expires_at_ms: credential.expires_at_ms,
        }
    } else if let Some(credential) = provider.request_credential(&account.id).await? {
        AuthProjection::Credential {
            secret: credential.secret,
            expires_at_ms: credential.expires_at_ms,
        }
    } else if let Some(provider) = ctx.env_isolated.get(account.provider.as_str()) {
        let env = provider
            .isolation_env(&account.id)
            .await?
            .map(|pairs| pairs.into_iter().collect::<BTreeMap<_, _>>())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "account {}:{} has no environment credential projection",
                    account.provider,
                    account.id
                )
            })?;
        AuthProjection::Environment { env }
    } else {
        bail!(
            "request credential export is not supported for provider `{}`",
            account.provider
        );
    };

    let output = AuthOutput {
        schema_version: 1,
        provider: account.provider,
        id: account.id.0,
        projection,
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
