//! `subswap rm <id|N>`：从 registry 与 keyring 删除账号，并从原生客户端登出/断开。
//!
//! 引用形式与 `subswap swap` 一致：数字编号 / id / label / `provider/id`，详见 [`crate::cmd::resolve_account`]。
//! **永不因 active / manual_only 拒绝。** 删的是某 provider 当前原生登录账号时尽力断原生；
//! 原生断失败只警告、仍清本地（客户端仍登录时下次入口可能再导入）。删 parked 账号只清本地。

use anyhow::Result;
use subswap_core::{paths::AppPaths, quota_collection, AuditEvent, OfficialDisconnect};

use crate::app::AppContext;
use crate::cmd::default::print_status_overview;
use crate::cmd::resolve_account;

pub async fn run(ctx: &AppContext, id_input: &str, json: bool) -> Result<()> {
    let acc = resolve_account(ctx, id_input)?;

    // 原生是各 provider 登录的权威源：尽力断原生，失败不挡本地删除。
    let official = match acc.provider.as_str() {
        "claude" => {
            try_official_disconnect("claude", ctx.claude.disconnect_official(&acc.id).await)
        }
        "codex" => try_official_disconnect("codex", ctx.codex.disconnect_official(&acc).await),
        "kimi" => try_official_disconnect("kimi", ctx.kimi.disconnect_official(&acc).await),
        "cursor" => {
            try_official_disconnect("cursor", ctx.cursor.disconnect_official(&acc.id).await)
        }
        "commandcode" => try_official_disconnect(
            "commandcode",
            ctx.commandcode.disconnect_official(&acc).await,
        ),
        "opencode-api-key" => try_official_disconnect(
            "opencode-api-key",
            ctx.opencode_api_key.disconnect_official(&acc.id).await,
        ),
        "opencode" => try_official_disconnect(
            "opencode",
            ctx.opencode.disconnect_official_console(&acc).await,
        ),
        "antigravity" => try_official_disconnect(
            "antigravity",
            ctx.antigravity.disconnect_official(&acc.id).await,
        ),
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
        "antigravity" => ctx.antigravity.live_account_id().ok(),
        _ => None,
    }
    .is_some_and(|live| live == acc.id);

    ctx.registry.remove(&acc.provider, &acc.id)?;

    // 清掉额度缓存与采集协调记录。失败只警告：rm 永不因缓存/协调状态拒绝删除。
    // 不删除协调锁或记录，而是保留未来的请求下限（含 Retry-After），避免同 ID 重新导入
    // 立即打上游；旧成功结果与在途采集结果都会在账号锁内作废。
    invalidate_quota_state(&acc.provider, &acc.id.0).await;

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

async fn invalidate_quota_state(provider: &str, account_id: &str) {
    let Ok(paths) = AppPaths::resolve() else {
        return;
    };
    if let Err(error) = quota_collection::invalidate_collection_state(
        &paths.quota_cache_file(),
        provider,
        account_id,
    )
    .await
    {
        tracing::warn!(
            err = %error,
            provider,
            account = account_id,
            "quota cache invalidation failed; continuing rm"
        );
        eprintln!(
            "warning: could not clear cached quota state for {provider}/{account_id} ({error}); local removal continues"
        );
    }
}

fn try_official_disconnect(
    provider: &str,
    result: Result<OfficialDisconnect, impl std::fmt::Display>,
) -> Option<OfficialDisconnect> {
    match result {
        Ok(status) => Some(status),
        Err(e) => {
            tracing::warn!(err = %e, provider, "official disconnect failed; continuing local rm");
            eprintln!("warning: could not sign out {provider} in the native client ({e}); local record removed anyway");
            None
        }
    }
}
