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
pub async fn run(ctx: &AppContext, input: &str) -> Result<()> {
    let account = resolve_account(ctx, input)?;
    let provider = ctx.providers.get(&account.provider)?;

    let projection = if let Some(credential) = provider.request_credential(&account.id).await? {
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
