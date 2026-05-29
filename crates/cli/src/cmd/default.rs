//! `subswap`（无参）默认入口。
//!
//! 流程：sync_local_active → 先渲染骨架 → 并发拉 quota 渐进刷新 → AutoSwapPolicy → 最终渲染。
//! 详见 docs/design/ARCHITECTURE.md §3.1。

use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::Result;
use futures::future::join_all;
use subswap_core::{
    auto_decide, settings, AccountId, AccountWithQuotas, AuditEvent, PolicyConfig, PolicyDecision,
    ProviderRegistry, ProviderSnapshot, Quota, QuotaFetchState,
};

use crate::app::AppContext;
use crate::daemon_spawn::ensure_daemon_running;
use crate::render::{
    account_ref, compact_error, compact_policy_reason, AutoLine, AutoLineKind, InlineRenderer,
};

pub async fn run(ctx: &AppContext) -> Result<()> {
    // 1. 自动 import 本地激活账号（如果没记录过）。
    sync_local_active(ctx);

    // 2. 先输出账号骨架，再随 quota 请求完成原地刷新。
    let interactive = io::stdout().is_terminal();
    let mut snapshots = build_loading_snapshots(&ctx.providers).await;
    let mut renderer = InlineRenderer::new(interactive);
    if interactive {
        renderer.render(&snapshots, &[])?;
    }
    fill_quotas_progressively(
        &ctx.providers,
        &mut snapshots,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
    )
    .await?;

    // 3. 应用 AutoSwapPolicy；只在完整 quota 返回后决策，避免半截数据乱切。
    let cfg = PolicyConfig::default();
    let mut auto_lines: Vec<AutoLine> = Vec::new();
    let mut activated: Vec<(String, AccountId)> = Vec::new();
    for snap in &snapshots {
        if snap.accounts.is_empty() {
            continue;
        }
        match auto_decide(snap, &cfg) {
            PolicyDecision::Swap { to, .. } => {
                let p = ctx.providers.get(&snap.provider)?;
                match p.activate(&to).await {
                    Ok(()) => {
                        auto_lines.push(AutoLine {
                            provider: snap.provider.clone(),
                            text: format!("auto: swapped to {}", account_ref(&to.0)),
                            kind: AutoLineKind::Info,
                        });
                        ctx.audit.append(AuditEvent::ok(
                            "auto_swap",
                            &snap.provider,
                            Some(to.0.as_str()),
                        ));
                        activated.push((snap.provider.clone(), to));
                    }
                    Err(e) => {
                        auto_lines.push(AutoLine {
                            provider: snap.provider.clone(),
                            text: format!("auto: failed ({})", compact_error(&e.to_string())),
                            kind: AutoLineKind::Error,
                        });
                        ctx.audit.append(AuditEvent::err(
                            "auto_swap",
                            &snap.provider,
                            Some(to.0.as_str()),
                            &e.to_string(),
                        ));
                    }
                }
            }
            PolicyDecision::Degraded { reason } => {
                tracing::debug!(
                    provider=%snap.provider,
                    reason=%compact_policy_reason(&reason),
                    "auto swap degraded"
                );
            }
            PolicyDecision::NoOp { .. } => {} // 沉默是金
        }
    }
    for (provider, id) in activated {
        mark_active(&mut snapshots, &provider, &id);
    }

    // 4. 最终渲染。交互场景刷新原输出块；非交互场景只输出最终版。
    renderer.render(&snapshots, &auto_lines)?;

    // 5. 后台保活:用户无感地拉起 daemon(已经在跑则什么都不做)。
    //    失败仅 debug 日志,不影响默认命令的退出码。
    if let Err(e) = ensure_daemon_running() {
        tracing::debug!(err = %e, "ensure_daemon_running failed; continuing");
    }
    Ok(())
}

/// 扫本地 ~/.claude / ~/.codex；如果有当前激活账号则 import 到 registry（已存在时 upsert）。
/// 任一 provider 失败（用户没登录过）静默跳过。
fn sync_local_active(ctx: &AppContext) {
    match ctx.claude.import_active(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("claude", &account.id) {
                tracing::debug!(err=%e, "skip claude active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip claude auto-import"),
    }
    match ctx.codex.sync_active_metadata(None) {
        Ok(account) => {
            if let Err(e) = ctx.registry.set_active("codex", &account.id) {
                tracing::debug!(err=%e, "skip codex active marker");
            }
        }
        Err(e) => tracing::debug!(err=%e, "skip codex auto-import"),
    }
}

async fn build_loading_snapshots(registry: &ProviderRegistry) -> Vec<ProviderSnapshot> {
    let provider_tasks = registry.all().into_iter().map(|p| async move {
        let provider = p.id().to_string();
        let accounts = p.list_accounts().await.unwrap_or_default();
        ProviderSnapshot {
            provider,
            accounts: accounts
                .into_iter()
                .map(|account| AccountWithQuotas {
                    account,
                    quotas: Vec::new(),
                    fetch_state: QuotaFetchState::Loading,
                })
                .collect(),
        }
    });
    join_all(provider_tasks).await
}

struct QuotaUpdate {
    provider: String,
    account_id: AccountId,
    result: std::result::Result<Vec<Quota>, String>,
}

async fn fill_quotas_progressively(
    registry: &ProviderRegistry,
    snapshots: &mut [ProviderSnapshot],
    mut renderer: Option<&mut InlineRenderer>,
) -> Result<()> {
    let total: usize = snapshots.iter().map(|snap| snap.accounts.len()).sum();
    if total == 0 {
        return Ok(());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(total);
    for snap in snapshots.iter() {
        let provider = snap.provider.clone();
        let p = registry.get(&provider)?;
        for awq in &snap.accounts {
            let tx = tx.clone();
            let p = p.clone();
            let provider = provider.clone();
            let account_id = awq.account.id.clone();
            tokio::spawn(async move {
                let result = p.query_quota(&account_id).await.map_err(|e| e.to_string());
                let _ = tx
                    .send(QuotaUpdate {
                        provider,
                        account_id,
                        result,
                    })
                    .await;
            });
        }
    }
    drop(tx);

    // 整体超时：超过 quota.fetch_timeout_ms 仍未返回的账号标记为超时失败，停止等待。
    // 动机：单个账号网络卡住不应拖住整条命令。已成功账号的结果不受影响。
    let timeout = Duration::from_millis(settings::current().quota.fetch_timeout_ms);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(update) => {
                    apply_quota_update(snapshots, update);
                    if let Some(renderer) = renderer.as_deref_mut() {
                        renderer.render(snapshots, &[])?;
                    }
                }
                None => break, // 全部账号已返回
            },
            _ = &mut deadline => {
                mark_pending_as_timed_out(snapshots);
                if let Some(renderer) = renderer.as_deref_mut() {
                    renderer.render(snapshots, &[])?;
                }
                break;
            }
        }
    }
    Ok(())
}

/// 把仍处于 `Loading`（超时未返回）的账号标记为超时失败。
fn mark_pending_as_timed_out(snapshots: &mut [ProviderSnapshot]) {
    for snap in snapshots.iter_mut() {
        for awq in &mut snap.accounts {
            if matches!(awq.fetch_state, QuotaFetchState::Loading) {
                awq.fetch_state = QuotaFetchState::Failed("quota fetch timeout".into());
            }
        }
    }
}

fn apply_quota_update(snapshots: &mut [ProviderSnapshot], update: QuotaUpdate) {
    let Some(snap) = snapshots
        .iter_mut()
        .find(|snap| snap.provider == update.provider)
    else {
        return;
    };
    let Some(awq) = snap
        .accounts
        .iter_mut()
        .find(|awq| awq.account.id == update.account_id)
    else {
        return;
    };
    match update.result {
        Ok(quotas) => {
            awq.quotas = quotas;
            awq.fetch_state = QuotaFetchState::Ready;
        }
        Err(err) => {
            awq.quotas.clear();
            awq.fetch_state = QuotaFetchState::Failed(err);
        }
    }
}

fn mark_active(snapshots: &mut [ProviderSnapshot], provider: &str, id: &AccountId) {
    for snap in snapshots {
        if snap.provider != provider {
            continue;
        }
        for awq in &mut snap.accounts {
            awq.account.active = awq.account.id == *id;
        }
    }
}
