//! `subswap rm <id|N>`：从 registry 与 keyring 删除账号。
//!
//! 引用形式与 `subswap swap` 一致：数字编号 / id / label / `provider/id`，详见 [`crate::cmd::resolve_account`]。
//! `opencode` / `opencode-api-key` 删前先断开官方客户端里的对应凭证，否则下次同步会导回。

use anyhow::{bail, Result};
use subswap_core::AuditEvent;
use subswap_provider_opencode::OfficialDisconnect;

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

    // opencode 系官方库是多凭证权威源：不断官方只清本地，下次同步必然导回。
    // 官方断失败直接报错退出、不清本地，避免“删了又回来”的假成功。
    let official = match acc.provider.as_str() {
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

    let fields: &[&str] = match acc.provider.as_str() {
        "claude" => &["credentials_json", "api_key"],
        "codex" => &["auth_json"],
        "cursor" | "kimi" | "opencode-api-key" | "commandcode" => &["blob"],
        _ => &[],
    };
    for f in fields {
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
            "also disconnected the official {} credential; it will not be re-imported",
            acc.provider
        );
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
