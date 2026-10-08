//! `subswap rm <id|N>`：从 registry 与 keyring 删除账号，并从原生客户端登出/断开。
//!
//! 引用形式与 `subswap swap` 一致：数字编号 / id / label / `provider/id`，详见 [`crate::cmd::resolve_account`]。
//! 删的是某 provider 当前原生登录账号时先断原生（否则下次同步导回）；原生断失败
//! 直接报错退出、不清本地。删 parked 账号只清本地。

use anyhow::{bail, Result};
use subswap_core::{AuditEvent, OfficialDisconnect};

use crate::app::AppContext;
use crate::cmd::default::print_status_overview;
use crate::cmd::resolve_account;

pub async fn run(ctx: &AppContext, id_input: &str, json: bool) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;
    if acc.provider == "claude" && acc.active && acc.manual_only() {
        bail!(
            "cannot remove active manual-only account {}/{}; swap away first",
            acc.provider,
            acc.id
        );
    }

    // 原生是各 provider 登录的权威源：删 live 账号不断原生，下次同步必然导回。
    // 原生断失败直接报错退出、不清本地，避免“删了又回来”的假成功。
    let official = match acc.provider.as_str() {
        "claude" => Some(ctx.claude.disconnect_official(&acc.id).await?),
        "codex" => Some(ctx.codex.disconnect_official(&acc).await?),
        "kimi" => Some(ctx.kimi.disconnect_official(&acc).await?),
        "cursor" => Some(ctx.cursor.disconnect_official(&acc.id).await?),
        "commandcode" => Some(ctx.commandcode.disconnect_official(&acc).await?),
        "opencode-api-key" => Some(ctx.opencode_api_key.disconnect_official(&acc.id).await?),
        "opencode" => Some(ctx.opencode.disconnect_official_console(&acc).await?),
        _ => None,
    };

    let still_signed_in = match acc.provider.as_str() {
        "claude" => ctx.claude.live_account_id().ok(),
        "codex" => ctx.codex.live_account_id().ok(),
        "kimi" => ctx.kimi.live_account_id().ok(),
        "cursor" => ctx.cursor.live_account_id().await.ok(),
        "opencode" => ctx.opencode.live_console_id().ok(),
        "opencode-api-key" => ctx.opencode_api_key.live_account_id().ok(),
        "commandcode" => ctx.commandcode.live_account_id().ok(),
        _ => None,
    }
    .is_some_and(|live| live == acc.id);

    ctx.registry.remove(&acc.provider, &acc.id)?;

    // 各 provider 的专属凭证字段由 provider 自己声明（`credential_store_fields`）。
    for f in ctx
        .providers
        .get(&acc.provider)
        .map(|p| p.credential_store_fields())
        .unwrap_or(&[])
    {
        if let Err(e) = ctx.store.delete(&acc.provider, acc.id.0.as_str(), f) {
            tracing::warn!(err=%e, field=%f, "keyring delete failed (continuing)");
        }
    }

    ctx.audit
        .append(AuditEvent::ok("rm", &acc.provider, Some(acc.id.0.as_str())));
    println!("removed {}/{}", acc.provider, acc.id);
    if official == Some(OfficialDisconnect::Disconnected) {
        ctx.audit.append(AuditEvent::ok(
            "rm_official_disconnect",
            &acc.provider,
            Some(acc.id.0.as_str()),
        ));
        println!(
            "also signed out {}/{} in the native client; it will not be re-imported",
            acc.provider, acc.id,
        );
        let quits_client = ctx
            .providers
            .get(&acc.provider)
            .map(|p| p.disconnect_quits_client())
            .unwrap_or(false);
        if quits_client {
            println!(
                "note: if the client was running, it was quit for the sign-out and was not relaunched"
            );
        }
    }
    if still_signed_in {
        println!(
            "note: {} is still signed in as this account; it will be picked up again on the next run — sign out in the client first to keep it out",
            acc.provider
        );
    }
    if !json {
        print_status_overview(ctx).await?;
    }
    Ok(())
}
