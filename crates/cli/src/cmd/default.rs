//! `subswap`（无参）默认入口，以及写操作成功后的状态面。
//!
//! 默认入口：sync_local_active → 先渲染骨架 → 并发拉 quota 渐进刷新 → per-provider AutoSwapPolicy → 最终渲染。
//! 写操作收尾：[`print_status_overview`] 只展示当前 registry，不 sync、不 AutoSwap、不拉 daemon。
//! 详见 docs/design/ARCHITECTURE.md §3.1、§3.3.1。

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::Result;
use futures::future::join_all;
use subswap_core::{
    auto_decide, is_authentication_failure, paths::AppPaths, query_quota_with_retry, settings,
    AccountId, AccountWithQuotas, AuditEvent, AuditLog, PolicyConfig, PolicyDecision,
    ProviderRegistry, ProviderSnapshot, Quota, QuotaCache, QuotaFetchState,
};

use crate::app::AppContext;
use crate::daemon_spawn::ensure_daemon_running;
use crate::render::{compact_error, compact_policy_reason, AutoLine, AutoLineKind, InlineRenderer};

pub async fn run(ctx: &AppContext, json: bool) -> Result<()> {
    // 1. 自动 import 本地激活账号（如果没记录过）；客户端明明登录着但同步失败的，
    //    收集成提示行随最终渲染一起打出来，不再静默吞掉。
    let mut auto_lines: Vec<AutoLine> = sync_local_active(ctx).await;

    // 2. 先输出账号骨架，再随 quota 请求完成原地刷新。
    // JSON 模式强制走非交互路径（不渲染 ANSI 骨架），最后统一以 JSON 输出。
    let interactive = !json && io::stdout().is_terminal();
    let mut snapshots = build_loading_snapshots(&ctx.providers).await;
    let mut renderer = InlineRenderer::new(interactive);
    if interactive {
        renderer.render(&snapshots, &auto_lines)?;
    }
    let cfg = PolicyConfig::default();
    fill_quotas_progressively(
        &ctx.providers,
        &ctx.audit,
        &mut snapshots,
        Some(&cfg),
        &mut auto_lines,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
        &quota_cache_path(),
    )
    .await?;

    // 3. 最终输出。JSON 模式吐结构化快照供程序消费；否则人类渲染（交互刷新原块 / 非交互出最终版）。
    if json {
        print_quota_json(&snapshots)?;
    } else {
        renderer.render(&snapshots, &auto_lines)?;
    }

    // 4. 后台保活:用户无感地拉起 daemon(已经在跑则什么都不做)。
    //    失败仅 debug 日志,不影响默认命令的退出码。
    if let Err(e) = ensure_daemon_running() {
        tracing::debug!(err = %e, "ensure_daemon_running failed; continuing");
    }
    Ok(())
}

/// 账号池写操作成功后回到与默认入口同一张余量表。
///
/// 只读当前 registry：不 `sync_local_active`（避免刚删的号被导回）、不 AutoSwap
/// （避免刚手动切走的号被顶掉）、不拉起 daemon。quota 仍走同一套缓存与节流。
pub async fn print_status_overview(ctx: &AppContext) -> Result<()> {
    println!();
    let interactive = io::stdout().is_terminal();
    let mut snapshots = build_loading_snapshots(&ctx.providers).await;
    let mut auto_lines = Vec::new();
    let mut renderer = InlineRenderer::new(interactive);
    if interactive {
        renderer.render(&snapshots, &auto_lines)?;
    }
    fill_quotas_progressively(
        &ctx.providers,
        &ctx.audit,
        &mut snapshots,
        None,
        &mut auto_lines,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
        &quota_cache_path(),
    )
    .await?;
    renderer.render(&snapshots, &auto_lines)?;
    Ok(())
}

fn quota_cache_path() -> std::path::PathBuf {
    AppPaths::resolve()
        .map(|p| p.quota_cache_file())
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/subswap_quota_cache.json"))
}

/// JSON 输出用 DTO：每个账号一条，含额度窗口与各自 reset_at，供程序（如 OpenConductor）消费。
#[derive(serde::Serialize)]
struct AccountQuotaJson {
    id: String,
    provider: String,
    label: String,
    active: bool,
    /// 计费方式：flat（订阅固定费率）| metered（按量计费）| unlimited（不限量）。
    /// 给 OpenConductor 等下游消费者判断"是否真花钱"并据此排权重。
    billing: String,
    /// quota 拉取状态：ready | loading | failed | stale。
    fetch_state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// 各窗口快照（Quota 自身可序列化，含 window / used / limit / reset_at / status）。
    quotas: Vec<Quota>,
}

/// 把账号 + 额度快照以 JSON 数组打到 stdout。
fn print_quota_json(snapshots: &[ProviderSnapshot]) -> Result<()> {
    let mut accounts = Vec::new();
    for snap in snapshots {
        for awq in &snap.accounts {
            let (fetch_state, error) = match &awq.fetch_state {
                QuotaFetchState::Loading => ("loading", None),
                QuotaFetchState::Ready => ("ready", None),
                QuotaFetchState::Failed(e) => ("failed", Some(e.clone())),
                QuotaFetchState::Stale { error, .. } => ("stale", Some(error.clone())),
            };
            accounts.push(AccountQuotaJson {
                id: awq.account.id.0.clone(),
                provider: awq.account.provider.clone(),
                label: awq.account.label.clone(),
                active: awq.account.active,
                billing: awq.account.billing().to_string(),
                fetch_state,
                error,
                quotas: awq.quotas.clone(),
            });
        }
    }
    println!("{}", serde_json::to_string_pretty(&accounts)?);
    Ok(())
}

fn auto_swap_success_text(snap: &ProviderSnapshot, to: &AccountId) -> String {
    let target = snap
        .accounts
        .iter()
        .find(|a| a.account.id == *to)
        .and_then(|a| {
            let label = a.account.label.trim();
            if label.is_empty() || label == a.account.id.0.as_str() {
                None
            } else {
                Some(label.to_string())
            }
        });

    let message = match target {
        Some(label) => format!("auto: swapped to {label}"),
        None => "auto: swapped".into(),
    };
    if snap.provider == "codex" {
        format!("{message}; restart running Codex CLI sessions to use it")
    } else {
        message
    }
}

/// 客户端确实登录着（`live_account_id` 命中）、但同步/导入失败时给出的提示行，
/// 取代过去的 `tracing::debug!` 静默吞掉——那种吞法正是「整段 provider 无声消失」反复出现的根因。
fn signed_in_but_untracked(
    provider: &str,
    id: &AccountId,
    err: impl std::fmt::Display,
) -> AutoLine {
    AutoLine {
        provider: provider.to_string(),
        text: format!(
            "signed in as {id} but not tracked ({}); run `subswap login {provider}`",
            compact_error(&err.to_string())
        ),
        kind: AutoLineKind::Error,
    }
}

/// 扫本地 ~/.claude / ~/.codex / ~/.cursor 等；如果有当前激活账号则 import 到 registry（已存在时 upsert）。
/// 客户端没登录过是正常状态，静默跳过；客户端明明登录着但同步失败的，收集成提示行返回给调用方渲染。
async fn sync_local_active(ctx: &AppContext) -> Vec<AutoLine> {
    if default_entry_avoids_keychain_sync() {
        return sync_local_active_metadata(ctx).await;
    }
    let mut notices = Vec::new();
    if let Ok(id) = ctx.claude.live_account_id() {
        match ctx.claude.import_active(None) {
            Ok(account) => {
                if ctx.registry.set_active("claude", &account.id).is_ok() {
                    clear_settled_marker(ctx, "claude", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("claude", &id, e)),
        }
    }
    if let Ok(id) = ctx.codex.live_account_id() {
        match ctx.codex.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("codex", &account.id).is_ok() {
                    clear_settled_marker(ctx, "codex", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("codex", &id, e)),
        }
    }
    if let Ok(id) = ctx.kimi.live_account_id() {
        match ctx.kimi.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("kimi", &account.id).is_ok() {
                    clear_settled_marker(ctx, "kimi", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("kimi", &id, e)),
        }
    }
    if let Ok(id) = ctx.cursor.live_account_id().await {
        match ctx.cursor.sync_active_metadata(None).await {
            Ok(account) => {
                if ctx.registry.set_active("cursor", &account.id).is_ok() {
                    clear_settled_marker(ctx, "cursor", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("cursor", &id, e)),
        }
    }
    notices.extend(sync_opencode_accounts(ctx).await);
    if let Ok(id) = ctx.commandcode.live_account_id() {
        match ctx.commandcode.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("commandcode", &account.id).is_ok() {
                    clear_settled_marker(ctx, "commandcode", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("commandcode", &id, e)),
        }
    }
    notices
}

/// 默认入口的 live 对齐只做「标记 active」，不产生切换语义：
/// 对齐后把新 active 的 `last_used_at` 清零，避免原生客户端里的外部切号
/// 被 settle-grace 误当成 subswap 刚做的切换而保护起来。
/// （manual-hold 只认 subswap 自己的 swap/login 写入，不受 last_used_at 影响。）
fn clear_settled_marker(ctx: &AppContext, provider: &str, id: &subswap_core::AccountId) {
    clear_settled_marker_in_registry(&ctx.registry, provider, id);
}

fn clear_settled_marker_in_registry(
    registry: &subswap_core::AccountRegistry,
    provider: &str,
    id: &subswap_core::AccountId,
) {
    let Ok(mut all) = registry.load() else {
        return;
    };
    let mut touched = false;
    for a in &mut all {
        if a.provider == provider && a.id == *id && a.last_used_at.is_some() {
            a.last_used_at = None;
            touched = true;
        }
    }
    if touched {
        if let Err(e) = registry.save(&all) {
            tracing::debug!(err=%e, provider=%provider, "clear settled marker failed");
        }
    }
}

async fn sync_opencode_accounts(ctx: &AppContext) -> Vec<AutoLine> {
    let key = ctx.opencode_api_key.clone();
    let console = ctx.opencode.clone();
    let registry = ctx.registry.clone();
    match tokio::task::spawn_blocking(move || {
        let mut notices = Vec::new();
        let key_live = key.live_account_id().ok();
        match key.sync_accounts(None) {
            Ok(accounts) => {
                if let Some(account) = accounts.into_iter().find(|a| a.active) {
                    if registry.set_active("opencode-api-key", &account.id).is_ok() {
                        clear_settled_marker_in_registry(
                            &registry,
                            "opencode-api-key",
                            &account.id,
                        );
                    }
                }
            }
            Err(e) => {
                if let Some(id) = key_live {
                    notices.push(signed_in_but_untracked("opencode-api-key", &id, e));
                }
            }
        }
        if let Ok(id) = console.live_console_id() {
            match console.sync_console_active_metadata(None) {
                Ok(account) => {
                    if registry.set_active("opencode", &account.id).is_ok() {
                        clear_settled_marker_in_registry(&registry, "opencode", &account.id);
                    }
                }
                Err(e) => notices.push(signed_in_but_untracked("opencode", &id, e)),
            }
        }
        notices
    })
    .await
    {
        Ok(notices) => notices,
        Err(e) => vec![AutoLine {
            provider: "opencode".into(),
            text: format!(
                "OpenCode login sync failed: {}",
                compact_error(&e.to_string())
            ),
            kind: AutoLineKind::Error,
        }],
    }
}

async fn sync_local_active_metadata(ctx: &AppContext) -> Vec<AutoLine> {
    let mut notices = Vec::new();
    if let Ok(id) = ctx.claude.live_account_id() {
        match ctx.claude.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("claude", &account.id).is_ok() {
                    clear_settled_marker(ctx, "claude", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("claude", &id, e)),
        }
    }
    if let Ok(id) = ctx.codex.live_account_id() {
        match ctx.codex.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("codex", &account.id).is_ok() {
                    clear_settled_marker(ctx, "codex", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("codex", &id, e)),
        }
    }
    if let Ok(id) = ctx.kimi.live_account_id() {
        match ctx.kimi.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("kimi", &account.id).is_ok() {
                    clear_settled_marker(ctx, "kimi", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("kimi", &id, e)),
        }
    }
    if let Ok(id) = ctx.cursor.live_account_id().await {
        match ctx.cursor.sync_active_metadata(None).await {
            Ok(account) => {
                if ctx.registry.set_active("cursor", &account.id).is_ok() {
                    clear_settled_marker(ctx, "cursor", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("cursor", &id, e)),
        }
    }
    notices.extend(sync_opencode_accounts(ctx).await);
    if let Ok(id) = ctx.commandcode.live_account_id() {
        match ctx.commandcode.sync_active_metadata(None) {
            Ok(account) => {
                if ctx.registry.set_active("commandcode", &account.id).is_ok() {
                    clear_settled_marker(ctx, "commandcode", &account.id);
                }
            }
            Err(e) => notices.push(signed_in_but_untracked("commandcode", &id, e)),
        }
    }
    notices
}

#[cfg(target_os = "macos")]
fn default_entry_avoids_keychain_sync() -> bool {
    std::env::var_os("SUBSWAP_SYNC_KEYCHAIN_ON_START").is_none()
}

#[cfg(not(target_os = "macos"))]
fn default_entry_avoids_keychain_sync() -> bool {
    false
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

/// 渐进式自动切换的运行内状态:额度是边查边回的,不能查到第一份就把决策锁死。
#[derive(Default)]
struct AutoSwapProgress {
    /// 本次运行内每个 provider 已切到的目标,避免重复 activate(重写凭证)。
    activated_targets: HashMap<String, AccountId>,
    /// 本次运行内主动离开过的账号,「只升级、不回头」防止 A→B→A 抖动。
    abandoned: HashMap<String, HashSet<AccountId>>,
}

async fn fill_quotas_progressively(
    registry: &ProviderRegistry,
    audit: &AuditLog,
    snapshots: &mut [ProviderSnapshot],
    auto_swap: Option<&PolicyConfig>,
    auto_lines: &mut Vec<AutoLine>,
    mut renderer: Option<&mut InlineRenderer>,
    cache_path: &std::path::Path,
) -> Result<()> {
    let total: usize = snapshots.iter().map(|snap| snap.accounts.len()).sum();
    if total == 0 {
        return Ok(());
    }
    let mut cache = QuotaCache::load(cache_path);
    let quota_cfg = settings::current().quota.clone();
    let min_refresh = Duration::from_millis(quota_cfg.min_refresh_interval_ms);
    let backoff_cap = Duration::from_millis(quota_cfg.failure_backoff_max_ms);

    let mut jobs = Vec::new();
    for snap in snapshots.iter_mut() {
        let provider = snap.provider.clone();
        for awq in &mut snap.accounts {
            // 缓存节流：缓存够新(< min_refresh)就直接复用、不打 usage 端点，避免高频触发 429。
            // daemon 与 CLI 共用 quota_cache.json，谁先查到谁刷新 cached_at，另一方据此跳过。
            if let Some(entry) = cache.fresh(&provider, &awq.account.id.0, min_refresh) {
                awq.quotas = entry.quotas;
                awq.fetch_state = QuotaFetchState::Ready;
                continue;
            }
            // 失败退避：连续查不出的账号在退避窗口内不再重打端点，直接沿用上次的失败呈现。
            if let Some(failure) =
                cache.in_failure_backoff(&provider, &awq.account.id.0, min_refresh, backoff_cap)
            {
                let error = failure.error.clone();
                let account_id = awq.account.id.0.clone();
                apply_quota_failure(awq, &cache, &provider, &account_id, error);
                continue;
            }
            // 凭证已走明文 FileStore，查任何账号都不再弹钥匙串，激活/非激活一律查额度。
            jobs.push((provider.clone(), awq.account.id.clone()));
        }
    }
    if let Some(renderer) = renderer.as_deref_mut() {
        renderer.render(snapshots, auto_lines)?;
    }
    if jobs.is_empty() {
        return Ok(());
    }

    let mut progress = AutoSwapProgress::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(jobs.len());
    for (provider, account_id) in jobs {
        let p = registry.get(&provider)?;
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = query_quota_with_retry(p.as_ref(), &account_id)
                .await
                .map_err(|e| e.to_string());
            let _ = tx
                .send(QuotaUpdate {
                    provider,
                    account_id,
                    result,
                })
                .await;
        });
    }
    drop(tx);

    while let Some(update) = rx.recv().await {
        let provider = update.provider.clone();
        apply_quota_update(snapshots, update, &mut cache);
        if let Some(cfg) = auto_swap {
            try_auto_swap_ready_provider(
                registry,
                audit,
                snapshots,
                &provider,
                cfg,
                auto_lines,
                &mut progress,
            )
            .await?;
        }
        if let Some(renderer) = renderer.as_deref_mut() {
            renderer.render(snapshots, auto_lines)?;
        }
    }
    cache.save(cache_path);
    Ok(())
}

/// 每收到一份额度就对该 provider 重判一次。
/// 当前账号已确认需切换、候选已确认可用时才激活；查询慢或失败不会导致中间跳转。
/// 切到可用账号后自然 NoOp，再用 activated_targets / abandoned 避免重复激活及回切。
async fn try_auto_swap_ready_provider(
    registry: &ProviderRegistry,
    audit: &AuditLog,
    snapshots: &mut [ProviderSnapshot],
    provider: &str,
    cfg: &PolicyConfig,
    auto_lines: &mut Vec<AutoLine>,
    progress: &mut AutoSwapProgress,
) -> Result<()> {
    let Some(index) = snapshots.iter().position(|snap| snap.provider == provider) else {
        return Ok(());
    };
    let snap = &snapshots[index];
    if snap.accounts.is_empty() {
        return Ok(());
    }

    let (from, to) = match auto_decide(snap, cfg) {
        PolicyDecision::Swap { from, to, .. } => (from, to),
        PolicyDecision::Degraded { reason } => {
            tracing::debug!(
                provider=%provider,
                reason=%compact_policy_reason(&reason),
                "auto swap degraded"
            );
            return Ok(());
        }
        // 沉默是金。额度可能还在补,下一份回来时会重判,不在此处锁死。
        // 手动保持是用户显式选择,值得露出一行(默认入口是用户看到保持的唯一地方)。
        PolicyDecision::NoOp { reason } if reason.contains("manually selected") => {
            set_auto_line(
                auto_lines,
                provider,
                format!("auto: held ({reason})"),
                AutoLineKind::Info,
            );
            return Ok(());
        }
        PolicyDecision::NoOp { .. } => return Ok(()),
    };

    // 已经切到过这个目标:无需重复 activate(重写凭证)。
    if progress.activated_targets.get(provider) == Some(&to) {
        return Ok(());
    }
    // 只升级、不回头:本次运行内主动离开过的账号不再切回,避免抖动。
    if progress
        .abandoned
        .get(provider)
        .is_some_and(|left| left.contains(&to))
    {
        return Ok(());
    }

    let p = registry.get(provider)?;
    let success_text = auto_swap_success_text(snap, &to);
    match p.activate(&to).await {
        Ok(()) => {
            set_auto_line(auto_lines, provider, success_text, AutoLineKind::Info);
            audit.append(AuditEvent::ok("auto_swap", provider, Some(to.0.as_str())));
            mark_active(snapshots, provider, &to);
            if let Some(from) = from {
                progress
                    .abandoned
                    .entry(provider.to_string())
                    .or_default()
                    .insert(from);
            }
            progress.activated_targets.insert(provider.to_string(), to);
        }
        Err(e) => {
            set_auto_line(
                auto_lines,
                provider,
                format!("auto: failed ({})", compact_error(&e.to_string())),
                AutoLineKind::Error,
            );
            audit.append(AuditEvent::err(
                "auto_swap",
                provider,
                Some(to.0.as_str()),
                &e.to_string(),
            ));
        }
    }

    Ok(())
}
/// 同一 provider 的自动切换提示原地替换,保证最终只展示一行最新结果
/// (例如先切逃生号、再升级到更优号时,只显示升级后的那条)。
fn set_auto_line(auto_lines: &mut Vec<AutoLine>, provider: &str, text: String, kind: AutoLineKind) {
    if let Some(line) = auto_lines.iter_mut().find(|l| l.provider == provider) {
        line.text = text;
        line.kind = kind;
    } else {
        auto_lines.push(AutoLine {
            provider: provider.to_string(),
            text,
            kind,
        });
    }
}

fn apply_quota_update(
    snapshots: &mut [ProviderSnapshot],
    update: QuotaUpdate,
    cache: &mut QuotaCache,
) {
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
            cache.set(&update.provider, &update.account_id.0, quotas.clone());
            awq.quotas = quotas;
            awq.fetch_state = QuotaFetchState::Ready;
        }
        Err(err) => {
            cache.record_failure(&update.provider, &update.account_id.0, &err);
            apply_quota_failure(awq, cache, &update.provider, &update.account_id.0, err);
        }
    }
}

/// 查询失败时的展示回落：瞬态失败（网络/429）有旧缓存就挂 `Stale`；
/// 鉴权/缺凭据是确定失败，丢掉旧数字只显示错误，避免「needs re-login」旁边还挂着串号留下的 0%。
fn apply_quota_failure(
    awq: &mut AccountWithQuotas,
    cache: &QuotaCache,
    provider: &str,
    account_id: &str,
    err: String,
) {
    if is_authentication_failure(&err) {
        awq.quotas.clear();
        awq.fetch_state = QuotaFetchState::Failed(err);
        return;
    }
    if let Some(entry) = cache.get(provider, account_id) {
        awq.quotas = entry.quotas;
        awq.fetch_state = QuotaFetchState::Stale {
            cached_at: entry.cached_at,
            error: err,
        };
    } else {
        awq.quotas.clear();
        awq.fetch_state = QuotaFetchState::Failed(err);
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use subswap_core::{
        Account, ClientTarget, Provider, QuotaFetchState, QuotaStatus, QuotaWindow,
    };
    use tokio::sync::{mpsc, Notify};

    fn snap_with_account(id: &str, label: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: "codex".into(),
            accounts: vec![AccountWithQuotas {
                account: Account {
                    provider: "codex".into(),
                    id: AccountId(id.into()),
                    label: label.into(),
                    active: false,
                    created_at: Utc::now(),
                    last_used_at: None,
                    priority: 100,
                    extra: serde_json::Map::new(),
                },
                quotas: Vec::new(),
                fetch_state: QuotaFetchState::Ready,
            }],
        }
    }

    #[test]
    fn auto_swap_success_text_uses_friendly_label() {
        let snap = snap_with_account(
            "c1311d9b-47d1-4b8b-95e9-3401f967abd6",
            "stromandanika707621@gmail.com",
        );

        assert_eq!(
            auto_swap_success_text(
                &snap,
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into())
            ),
            "auto: swapped to stromandanika707621@gmail.com; restart running Codex CLI sessions to use it"
        );
    }

    #[test]
    fn auto_swap_success_text_hides_raw_id_without_label() {
        let snap = snap_with_account(
            "c1311d9b-47d1-4b8b-95e9-3401f967abd6",
            "c1311d9b-47d1-4b8b-95e9-3401f967abd6",
        );

        assert_eq!(
            auto_swap_success_text(
                &snap,
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into())
            ),
            "auto: swapped; restart running Codex CLI sessions to use it"
        );
    }

    struct MockProvider {
        id: &'static str,
        accounts: Vec<Account>,
        quotas: HashMap<String, Vec<Quota>>,
        wait_for_quota: Option<Arc<Notify>>,
        wait_by_account: HashMap<String, Arc<Notify>>,
        // 这些账号的 quota 查询返回 Err,模拟拉取失败 → fetch_state=Failed。
        fail_accounts: HashSet<String>,
        activated: mpsc::UnboundedSender<(String, String)>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn display_name(&self) -> &'static str {
            self.id
        }

        fn client_targets(&self) -> Vec<ClientTarget> {
            Vec::new()
        }

        async fn list_accounts(&self) -> subswap_core::Result<Vec<Account>> {
            Ok(self.accounts.clone())
        }

        async fn activate(&self, id: &AccountId) -> subswap_core::Result<()> {
            let _ = self.activated.send((self.id.to_string(), id.0.clone()));
            Ok(())
        }

        async fn query_quota(&self, id: &AccountId) -> subswap_core::Result<Vec<Quota>> {
            if let Some(wait_for_quota) = self.wait_by_account.get(&id.0) {
                wait_for_quota.notified().await;
            } else if let Some(wait_for_quota) = &self.wait_for_quota {
                wait_for_quota.notified().await;
            }
            if self.fail_accounts.contains(&id.0) {
                // 用非重试错误(429),让失败快速落地为 Failed,不被 query_quota_with_retry
                // 的指数退避拖慢——否则测试里 escape 的失败状态会迟迟不到。
                return Err(subswap_core::Error::QuotaFetch(
                    "usage returned 429 too many requests".into(),
                ));
            }
            Ok(self.quotas.get(&id.0).cloned().unwrap_or_default())
        }
    }

    fn account(provider: &str, id: &str, active: bool) -> Account {
        Account {
            provider: provider.into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: serde_json::Map::new(),
        }
    }

    fn quota(provider: &str, id: &str, used: u64, status: QuotaStatus) -> Quota {
        Quota {
            provider: provider.into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used,
            limit: 100,
            reset_at: None,
            status,
            note: None,
        }
    }

    #[tokio::test]
    async fn ready_provider_auto_swaps_and_never_reports_isolated_session_skip() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        let slow_claude = Arc::new(Notify::new());

        let mut codex_quotas = HashMap::new();
        codex_quotas.insert(
            "codex-active".into(),
            vec![quota("codex", "codex-active", 99, QuotaStatus::Warn)],
        );
        codex_quotas.insert(
            "codex-candidate".into(),
            vec![quota("codex", "codex-candidate", 1, QuotaStatus::Ok)],
        );

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "claude",
            accounts: vec![account("claude", "claude-active", true)],
            quotas: HashMap::new(),
            wait_for_quota: Some(slow_claude.clone()),
            wait_by_account: HashMap::new(),
            fail_accounts: HashSet::new(),
            activated: activated_tx.clone(),
        }));
        registry.register(Arc::new(MockProvider {
            id: "codex",
            accounts: vec![
                account("codex", "codex-active", true),
                account("codex", "codex-candidate", false),
            ],
            quotas: codex_quotas,
            wait_for_quota: None,
            wait_by_account: HashMap::new(),
            fail_accounts: HashSet::new(),
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            settle_grace_ms: 60_000,
            manual_hold_ms: 0,
        };
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            fill_quotas_progressively(
                &registry,
                &audit,
                &mut snapshots,
                Some(&cfg),
                &mut auto_lines,
                None,
                &cache_path,
            )
            .await
            .unwrap();
            (snapshots, auto_lines)
        });

        let activated = tokio::time::timeout(Duration::from_millis(300), activated_rx.recv())
            .await
            .expect("codex should activate before claude quota finishes")
            .expect("activation channel should stay open");
        assert_eq!(
            activated,
            ("codex".to_string(), "codex-candidate".to_string())
        );

        slow_claude.notify_waiters();
        let (snapshots, auto_lines) = handle.await.unwrap();
        let codex = snapshots
            .iter()
            .find(|snap| snap.provider == "codex")
            .unwrap();
        assert!(codex
            .accounts
            .iter()
            .any(|account| account.account.id.0 == "codex-candidate" && account.account.active));
        assert_eq!(auto_lines.len(), 1);
        assert_eq!(auto_lines[0].provider, "codex");
        assert!(
            !auto_lines[0].text.contains("isolated session active"),
            "auto swap must activate instead of skipping for an isolated session: {}",
            auto_lines[0].text
        );
    }

    #[tokio::test]
    async fn fill_quotas_without_auto_swap_does_not_activate() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        let mut quotas = HashMap::new();
        quotas.insert(
            "codex-active".into(),
            vec![quota("codex", "codex-active", 99, QuotaStatus::Warn)],
        );
        quotas.insert(
            "codex-candidate".into(),
            vec![quota("codex", "codex-candidate", 1, QuotaStatus::Ok)],
        );

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "codex",
            accounts: vec![
                account("codex", "codex-active", true),
                account("codex", "codex-candidate", false),
            ],
            quotas,
            wait_for_quota: None,
            wait_by_account: HashMap::new(),
            fail_accounts: HashSet::new(),
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();
        let cache_path = tmp.path().join("quota_cache.json");
        fill_quotas_progressively(
            &registry,
            &audit,
            &mut snapshots,
            None,
            &mut auto_lines,
            None,
            &cache_path,
        )
        .await
        .unwrap();

        assert!(
            activated_rx.try_recv().is_err(),
            "status overview must not auto-swap"
        );
        assert!(auto_lines.is_empty());
        let codex = snapshots
            .iter()
            .find(|snap| snap.provider == "codex")
            .unwrap();
        assert!(codex
            .accounts
            .iter()
            .any(|account| account.account.id.0 == "codex-active" && account.account.active));
    }

    #[tokio::test]
    async fn healthy_active_waits_for_its_own_quota_without_swapping() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        let slow_active = Arc::new(Notify::new());

        let mut quotas = HashMap::new();
        quotas.insert(
            "active".into(),
            vec![quota("opencode", "active", 10, QuotaStatus::Ok)],
        );
        quotas.insert(
            "candidate".into(),
            vec![quota("opencode", "candidate", 0, QuotaStatus::Ok)],
        );

        let mut wait_by_account = HashMap::new();
        wait_by_account.insert("active".into(), slow_active.clone());

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "opencode",
            accounts: vec![
                account("opencode", "active", true),
                account("opencode", "candidate", false),
            ],
            quotas,
            wait_for_quota: None,
            wait_by_account,
            fail_accounts: HashSet::new(),
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            settle_grace_ms: 60_000,
            manual_hold_ms: 0,
        };
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            fill_quotas_progressively(
                &registry,
                &audit,
                &mut snapshots,
                Some(&cfg),
                &mut auto_lines,
                None,
                &cache_path,
            )
            .await
            .unwrap();
            (snapshots, auto_lines)
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(100), activated_rx.recv())
                .await
                .is_err(),
            "healthy candidate must not replace a loading active account"
        );
        slow_active.notify_one();
        let (snapshots, auto_lines) = handle.await.unwrap();
        let opencode = snapshots
            .iter()
            .find(|snap| snap.provider == "opencode")
            .unwrap();
        assert!(opencode
            .accounts
            .iter()
            .any(|account| account.account.id.0 == "active" && account.account.active));
        assert!(auto_lines.is_empty());
        assert!(activated_rx.try_recv().is_err());
    }

    /// 已耗尽时等待已确认可用的候选，不先跳到查询失败的账号。
    #[tokio::test]
    async fn waits_for_usable_candidate_without_intermediate_failed_swap() {
        let (activated_tx, mut activated_rx) = mpsc::unbounded_channel();
        // 放慢可用候选，确保先收到耗尽与失败结果。
        let slow_better = Arc::new(Notify::new());

        let mut quotas = HashMap::new();
        // active 已耗尽，但没有可用候选前应保持原号。
        quotas.insert(
            "active".into(),
            vec![quota("claude", "active", 100, QuotaStatus::Exhausted)],
        );
        // better 真正可用,但额度回得慢。
        quotas.insert(
            "better".into(),
            vec![quota("claude", "better", 0, QuotaStatus::Ok)],
        );
        // 查询失败的账号不能成为自动候选。
        let mut wait_by_account = HashMap::new();
        wait_by_account.insert("better".into(), slow_better.clone());
        let mut fail_accounts = HashSet::new();
        fail_accounts.insert("escape".to_string());

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "claude",
            accounts: vec![
                account("claude", "active", true),
                account("claude", "escape", false),
                account("claude", "better", false),
            ],
            quotas,
            wait_for_quota: None,
            wait_by_account,
            fail_accounts,
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let cfg = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            settle_grace_ms: 0,
            manual_hold_ms: 0,
        };
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            fill_quotas_progressively(
                &registry,
                &audit,
                &mut snapshots,
                Some(&cfg),
                &mut auto_lines,
                None,
                &cache_path,
            )
            .await
            .unwrap();
            (snapshots, auto_lines)
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(100), activated_rx.recv())
                .await
                .is_err(),
            "must not switch to an unconfirmed fallback"
        );

        slow_better.notify_one();
        let selected = tokio::time::timeout(Duration::from_millis(300), activated_rx.recv())
            .await
            .expect("should switch once a usable candidate arrives")
            .expect("activation channel open");
        assert_eq!(selected, ("claude".to_string(), "better".to_string()));

        let (snapshots, auto_lines) = handle.await.unwrap();
        let claude = snapshots
            .iter()
            .find(|snap| snap.provider == "claude")
            .unwrap();
        assert!(claude
            .accounts
            .iter()
            .any(|a| a.account.id.0 == "better" && a.account.active));
        // 升级后不再切回已离开的 escape / active。
        assert!(claude
            .accounts
            .iter()
            .all(|a| a.account.id.0 == "better" || !a.account.active));
        // 只进行一次有效切换。
        assert!(activated_rx.try_recv().is_err());
        assert_eq!(auto_lines.len(), 1);
        assert_eq!(auto_lines[0].provider, "claude");
    }
}
