//! `subswap`（无参）默认入口，以及写操作成功后的状态面。
//!
//! 默认入口：sync_local_active → 先渲染骨架 → 并发拉 quota 渐进刷新 → per-provider AutoSwapPolicy → 最终渲染。
//! 写操作收尾：[`print_status_overview`] 只展示当前 registry，不 sync、不 AutoSwap、不拉 daemon。
//! 详见 docs/design/ARCHITECTURE.md §3.1、§3.3.1。

use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use futures::future::join_all;
use futures::FutureExt;
use subswap_core::quota_schedule::{fetch_state_for_observation, ScheduleConfig};
use subswap_core::{
    auto_decide, collect_quota, collect_quota_at, is_authentication_failure, paths::AppPaths,
    settings, AccountId, AccountWithQuotas, AuditEvent, AuditLog, PolicyConfig, PolicyDecision,
    ProviderRegistry, ProviderSnapshot, Quota, QuotaCache, QuotaFetchState, QuotaReading,
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
    let cache_path = quota_cache_path();
    fill_quotas_progressively(
        &QuotaFillContext {
            registry: &ctx.providers,
            audit: &ctx.audit,
            cache_path: &cache_path,
            observer: None,
        },
        &mut snapshots,
        Some(&cfg),
        &mut auto_lines,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
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
    let cache_path = quota_cache_path();
    fill_quotas_progressively(
        &QuotaFillContext {
            registry: &ctx.providers,
            audit: &ctx.audit,
            cache_path: &cache_path,
            observer: None,
        },
        &mut snapshots,
        None,
        &mut auto_lines,
        if interactive {
            Some(&mut renderer)
        } else {
            None
        },
    )
    .await?;
    renderer.render(&snapshots, &auto_lines)?;
    Ok(())
}

/// 读取配额缓存；文件 IO 放到阻塞线程，避免在 async 路径里直接读盘。
async fn load_quota_cache(path: &std::path::Path) -> QuotaCache {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || QuotaCache::load(&path))
        .await
        .unwrap_or_default()
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

fn auto_swap_success_text(
    snap: &ProviderSnapshot,
    to: &AccountId,
    post_swap_notice: Option<&str>,
) -> String {
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
    match post_swap_notice {
        Some(notice) => format!("{message}; {notice}"),
        None => message,
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
            pool_semantics: p.quota_pool_semantics(),
        }
    });
    join_all(provider_tasks).await
}

struct QuotaUpdate {
    provider: String,
    account_id: AccountId,
    result: std::result::Result<QuotaReading, String>,
}

/// 单个账号结果已应用到内存快照（并完成该 provider 的自动切换重判）后的测试观测。
///
/// 生产路径不订阅；测试用它作为确定性屏障，避免用固定 sleep 猜测「active/escape 是否已落地」。
/// 元组字段：`(provider, account_id, fetch_state, quota_windows)`。
type AppliedQuotaState = (String, String, QuotaFetchState, usize);

/// 渐进式额度填充的共享只读依赖。
///
/// 收在一起避免参数数量随观测钩子增长；生产路径 `observer` 为 `None`。
struct QuotaFillContext<'a> {
    registry: &'a ProviderRegistry,
    audit: &'a AuditLog,
    cache_path: &'a std::path::Path,
    observer: Option<&'a tokio::sync::mpsc::UnboundedSender<AppliedQuotaState>>,
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
    ctx: &QuotaFillContext<'_>,
    snapshots: &mut [ProviderSnapshot],
    auto_swap: Option<&PolicyConfig>,
    auto_lines: &mut Vec<AutoLine>,
    mut renderer: Option<&mut InlineRenderer>,
) -> Result<()> {
    let registry = ctx.registry;
    let audit = ctx.audit;
    let cache_path = ctx.cache_path;
    let total: usize = snapshots.iter().map(|snap| snap.accounts.len()).sum();
    if total == 0 {
        return Ok(());
    }
    let all_settings = settings::current();
    let quota_cfg = all_settings.quota.clone();
    let schedule_cfg = ScheduleConfig::from_settings(&quota_cfg);
    let min_refresh = Duration::from_millis(quota_cfg.min_refresh_interval_ms);
    let backoff_cap = Duration::from_millis(quota_cfg.failure_backoff_max_ms);
    let cache = load_quota_cache(cache_path).await;
    let now = Utc::now();

    // 先展示当前持久结果：新鲜数据 Ready；超过决策年龄的数据只能是 Stale，
    // 不能因为这次调度暂时没查就继续伪装成 Ready。
    for snap in snapshots.iter_mut() {
        for awq in &mut snap.accounts {
            merge_cached_state(awq, &cache, &snap.provider, &schedule_cfg, now);
        }
    }

    let mut jobs = Vec::new();
    for snap in snapshots.iter_mut() {
        let provider = snap.provider.clone();
        for awq in &mut snap.accounts {
            // 前台手动检查可以比后台节奏更早发起，但协调器仍会执行真实请求下限；
            // 缓存比 min_refresh 新时完全不用再打扰协调器。
            if cache
                .fresh(&provider, &awq.account.id.0, min_refresh)
                .is_some()
            {
                continue;
            }
            // 失败退避由协调器持久化；这里只沿用已有结果，不重复记录失败或重打端点。
            if cache
                .in_failure_backoff(&provider, &awq.account.id.0, min_refresh, backoff_cap)
                .is_some()
            {
                continue;
            }
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
    // 生产路径必须走会记录遥测的 `collect_quota`；显式临时缓存路径（测试/嵌入）
    // 才走 hermetic 的 `collect_quota_at`，避免碰到真实用户目录。
    let production_cache = AppPaths::resolve()
        .ok()
        .is_some_and(|paths| paths.quota_cache_file() == cache_path);
    // 前台仍然并发（跨 Provider），但同一 Provider 的账号串行处理：采集协调器默认
    // 每 Provider 只允许一个真实请求，串行顺序也让渐进结果与测试闸门可确定复现。
    let permits = Arc::new(tokio::sync::Semaphore::new(
        schedule_cfg.max_concurrent_fetches.max(1),
    ));
    let mut jobs_by_provider: HashMap<String, Vec<AccountId>> = HashMap::new();
    for (provider, account_id) in jobs {
        jobs_by_provider
            .entry(provider)
            .or_default()
            .push(account_id);
    }
    for (provider, account_ids) in jobs_by_provider {
        let p = registry.get(&provider)?;
        let tx = tx.clone();
        let permits = permits.clone();
        let cache_path = cache_path.to_path_buf();
        let settings = all_settings.clone();
        tokio::spawn(async move {
            for account_id in account_ids {
                let permit = match permits.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => break,
                };
                let provider_for_update = provider.clone();
                let account_for_update = account_id.clone();
                // 采集器 panic 不能静默丢掉这个账号的结果（否则接收端会在没有 update 的
                // 情况下结束，调用方误以为「本来就不需要它」）。收敛成该账号的失败结果。
                let result = AssertUnwindSafe(async {
                    if production_cache {
                        collect_quota(p.as_ref(), &account_id).await
                    } else {
                        collect_quota_at(p.as_ref(), &account_id, &cache_path, &settings).await
                    }
                })
                .catch_unwind()
                .await
                .map_err(|payload| {
                    format!(
                        "quota collection task panicked: {}",
                        panic_payload_message(payload.as_ref())
                    )
                })
                .and_then(|inner| inner.map_err(|e| e.to_string()));
                drop(permit);
                let _ = tx
                    .send(QuotaUpdate {
                        provider: provider_for_update,
                        account_id: account_for_update,
                        result,
                    })
                    .await;
            }
        });
    }
    drop(tx);

    while let Some(update) = rx.recv().await {
        let provider = update.provider.clone();
        let account_id = update.account_id.clone();
        // 协调器可能刚写了本轮真实结果；按账号重读缓存用于失败回退与更新事件比较。
        let cache = load_quota_cache(cache_path).await;
        apply_quota_update(snapshots, update, &cache, &schedule_cfg);
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
        if let Some(observer) = ctx.observer {
            if let Some(account) = snapshots
                .iter()
                .find(|snap| snap.provider == provider)
                .and_then(|snap| {
                    snap.accounts
                        .iter()
                        .find(|account| account.account.id == account_id)
                })
            {
                let _ = observer.send((
                    provider.clone(),
                    account_id.0.clone(),
                    account.fetch_state.clone(),
                    account.quotas.len(),
                ));
            }
        }
        if let Some(renderer) = renderer.as_deref_mut() {
            renderer.render(snapshots, auto_lines)?;
        }
    }
    Ok(())
}

/// 把 `catch_unwind` 的 panic payload 转成可记录的一行文本。
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// 把磁盘上最近一次确认结果并回内存快照。
///
/// 只读、不写缓存：持久化由共享采集协调器负责。成功缓存与失败记录同时存在时保留
/// `Stale { cached_at, error }`，既不丢旧数字，也不把错误吞成 Ready。
fn merge_cached_state(
    awq: &mut AccountWithQuotas,
    cache: &QuotaCache,
    provider: &str,
    cfg: &ScheduleConfig,
    now: chrono::DateTime<chrono::Utc>,
) {
    let account_id = awq.account.id.0.clone();
    let failure = cache.failure(provider, &account_id);
    match cache.get(provider, &account_id) {
        Some(entry) => {
            awq.quotas = entry.quotas;
            if let Some(failure) = failure {
                if is_authentication_failure(&failure.error) {
                    awq.quotas.clear();
                    awq.fetch_state = QuotaFetchState::Failed(failure.error.clone());
                } else {
                    awq.fetch_state = QuotaFetchState::Stale {
                        cached_at: entry.cached_at,
                        error: failure.error.clone(),
                    };
                }
            } else {
                awq.fetch_state = fetch_state_for_observation(Some(entry.cached_at), now, cfg);
            }
        }
        None => {
            awq.quotas.clear();
            awq.fetch_state = match failure {
                Some(failure) => QuotaFetchState::Failed(failure.error.clone()),
                None => QuotaFetchState::Loading,
            };
        }
    }
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
    let success_text = auto_swap_success_text(snap, &to, p.post_swap_notice());
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
    cache: &QuotaCache,
    cfg: &ScheduleConfig,
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
    let provider = update.provider.clone();
    let account_id = update.account_id.0.clone();
    match update.result {
        Ok(reading) => {
            // 协调器已经持久化这次真实结果（含原始 observed_at）；这里只读回填，
            // 不重写缓存时间戳，也不伪造一次「刚刚刷新」。
            let newer_failure = cache
                .failure(&provider, &account_id)
                .filter(|failure| failure.failed_at > reading.observed_at);
            awq.fetch_state = match newer_failure {
                Some(failure) if is_authentication_failure(&failure.error) => {
                    awq.quotas.clear();
                    QuotaFetchState::Failed(failure.error.clone())
                }
                Some(failure) => QuotaFetchState::Stale {
                    cached_at: reading.observed_at,
                    error: failure.error.clone(),
                },
                None => fetch_state_for_observation(Some(reading.observed_at), Utc::now(), cfg),
            };
            if !matches!(&awq.fetch_state, QuotaFetchState::Failed(_)) {
                awq.quotas = reading.quotas;
            }
        }
        Err(err) => {
            // 失败已由协调器记账；用刚重读的缓存回填，调用方不再 record_failure。
            apply_quota_failure(awq, cache, &provider, &account_id, err);
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
        Account, ClientTarget, Provider, QuotaFetchState, QuotaPoolSemantics, QuotaStatus,
        QuotaWindow,
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
            pool_semantics: QuotaPoolSemantics::Stacked,
        }
    }

    #[test]
    fn cached_quota_older_than_decision_age_is_stale() {
        let cfg = ScheduleConfig::from_settings(&settings::Quota::default());
        let now = Utc::now();
        let mut cache = QuotaCache::default();
        cache.set_at(
            "codex",
            "old",
            vec![quota("codex", "old", 10, QuotaStatus::Ok)],
            now - chrono::Duration::minutes(10),
        );
        let mut account = snap_with_account("old", "old").accounts.remove(0);
        merge_cached_state(&mut account, &cache, "codex", &cfg, now);
        assert_eq!(
            account.quotas.len(),
            1,
            "old numbers may still be displayed"
        );
        assert!(
            matches!(account.fetch_state, QuotaFetchState::Stale { .. }),
            "scheduler wait must not promote old data to Ready: {:?}",
            account.fetch_state
        );

        cache.set_at(
            "codex",
            "fresh",
            vec![quota("codex", "fresh", 10, QuotaStatus::Ok)],
            now - chrono::Duration::seconds(30),
        );
        let mut account = snap_with_account("fresh", "fresh").accounts.remove(0);
        merge_cached_state(&mut account, &cache, "codex", &cfg, now);
        assert_eq!(account.fetch_state, QuotaFetchState::Ready);
    }

    #[test]
    fn auto_swap_success_text_uses_friendly_label() {
        let snap = snap_with_account("c1311d9b-47d1-4b8b-95e9-3401f967abd6", "user@example.com");

        assert_eq!(
            auto_swap_success_text(
                &snap,
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into()),
                Some("Restart running Codex CLI sessions to use this account."),
            ),
            "auto: swapped to user@example.com; Restart running Codex CLI sessions to use this account."
        );
        assert_eq!(
            auto_swap_success_text(
                &snap,
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into()),
                None,
            ),
            "auto: swapped to user@example.com"
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
                &AccountId("c1311d9b-47d1-4b8b-95e9-3401f967abd6".into()),
                Some("Restart running Codex CLI sessions to use this account."),
            ),
            "auto: swapped; Restart running Codex CLI sessions to use this account."
        );
    }

    /// 单个账号查询的确定性闸门：`started` 表示查询已进入，`release` 放行返回。
    ///
    /// 测试用 `started` 做「查询已在途」屏障，用 `release` 放行，而不是 sleep 猜时序。
    #[derive(Default)]
    struct QueryGate {
        started: Notify,
        release: Notify,
    }

    impl QueryGate {
        async fn enter(&self) {
            // notify_one 会保存 permit，即使测试尚未 await started 也不会丢信号。
            self.started.notify_one();
            self.release.notified().await;
        }

        fn release(&self) {
            self.release.notify_one();
        }
    }

    struct MockProvider {
        id: &'static str,
        accounts: Vec<Account>,
        quotas: HashMap<String, Vec<Quota>>,
        gates: HashMap<String, Arc<QueryGate>>,
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
            if let Some(gate) = self.gates.get(&id.0) {
                gate.enter().await;
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
        let claude_gate = Arc::new(QueryGate::default());

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
            gates: HashMap::from([("claude-active".to_string(), claude_gate.clone())]),
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
            gates: HashMap::new(),
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
            let fill_ctx = QuotaFillContext {
                registry: &registry,
                audit: &audit,
                cache_path: &cache_path,
                observer: None,
            };
            fill_quotas_progressively(&fill_ctx, &mut snapshots, Some(&cfg), &mut auto_lines, None)
                .await
                .unwrap();
            (snapshots, auto_lines)
        });

        // 先等 Claude 查询真正进入（gate 已持有），再要求 codex 在 Claude 未返回前完成切换。
        // 10s 只是死锁保护，不是语义时序；Claude 未 release 前 fill 不会结束。
        tokio::time::timeout(Duration::from_secs(10), claude_gate.started.notified())
            .await
            .expect("claude quota query should start");
        let activated = tokio::time::timeout(Duration::from_secs(10), activated_rx.recv())
            .await
            .expect("codex should activate while claude quota is gated")
            .expect("activation channel should stay open");
        assert_eq!(
            activated,
            ("codex".to_string(), "codex-candidate".to_string())
        );

        claude_gate.release();
        let (snapshots, auto_lines) =
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("fill task should finish after releasing claude")
                .unwrap();
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
            gates: HashMap::new(),
            fail_accounts: HashSet::new(),
            activated: activated_tx,
        }));

        let mut snapshots = build_loading_snapshots(&registry).await;
        let tmp = tempfile::tempdir().unwrap();
        let audit = AuditLog::new(tmp.path().join("audit.log"));
        let mut auto_lines = Vec::new();
        let cache_path = tmp.path().join("quota_cache.json");
        let fill_ctx = QuotaFillContext {
            registry: &registry,
            audit: &audit,
            cache_path: &cache_path,
            observer: None,
        };
        fill_quotas_progressively(&fill_ctx, &mut snapshots, None, &mut auto_lines, None)
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
        let mut quotas = HashMap::new();
        quotas.insert(
            "active".into(),
            vec![quota("opencode", "active", 10, QuotaStatus::Ok)],
        );
        quotas.insert(
            "candidate".into(),
            vec![quota("opencode", "candidate", 0, QuotaStatus::Ok)],
        );

        let active_gate = Arc::new(QueryGate::default());
        let mut gates = HashMap::new();
        gates.insert("active".to_string(), active_gate.clone());

        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider {
            id: "opencode",
            accounts: vec![
                account("opencode", "active", true),
                account("opencode", "candidate", false),
            ],
            quotas,
            gates,
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
            let fill_ctx = QuotaFillContext {
                registry: &registry,
                audit: &audit,
                cache_path: &cache_path,
                observer: None,
            };
            fill_quotas_progressively(&fill_ctx, &mut snapshots, Some(&cfg), &mut auto_lines, None)
                .await
                .unwrap();
            (snapshots, auto_lines)
        });

        tokio::time::timeout(Duration::from_secs(10), active_gate.started.notified())
            .await
            .expect("active quota query should start");
        assert!(
            activated_rx.try_recv().is_err(),
            "healthy candidate must not replace a loading active account"
        );
        active_gate.release();
        let (snapshots, auto_lines) =
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("fill task should finish after releasing active")
                .unwrap();
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
        let better_gate = Arc::new(QueryGate::default());
        let mut gates = HashMap::new();
        gates.insert("better".to_string(), better_gate.clone());
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
            gates,
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
        let (applied_tx, mut applied_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            let cache_path = tmp.path().join("quota_cache.json");
            let _tmp = tmp;
            let fill_ctx = QuotaFillContext {
                registry: &registry,
                audit: &audit,
                cache_path: &cache_path,
                observer: Some(&applied_tx),
            };
            fill_quotas_progressively(&fill_ctx, &mut snapshots, Some(&cfg), &mut auto_lines, None)
                .await
                .unwrap();
            (snapshots, auto_lines)
        });

        // 等 active（耗尽）与 escape（查询失败）两个结果都真正应用到快照后再放行 better。
        // 这样「没有中间失败切换」不依赖 100ms 文件 IO/调度时序。
        let mut applied = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), applied_rx.recv())
                .await
                .expect("active and escape outcomes should be applied")
                .expect("applied observer channel should stay open");
            applied.push(event);
            if applied.iter().any(|event| event.1 == "active")
                && applied.iter().any(|event| event.1 == "escape")
            {
                break;
            }
        }
        let active_state = applied
            .iter()
            .find(|event| event.1 == "active")
            .map(|event| event.2.clone());
        let escape_state = applied
            .iter()
            .find(|event| event.1 == "escape")
            .map(|event| event.2.clone());
        assert!(
            matches!(active_state, Some(QuotaFetchState::Ready)),
            "active quota must be a completed reading; applied={applied:?}"
        );
        assert!(
            matches!(escape_state, Some(QuotaFetchState::Failed(_))),
            "escape must be a failed reading, not a candidate; applied={applied:?}"
        );
        assert!(
            activated_rx.try_recv().is_err(),
            "must not switch to an unconfirmed fallback; applied={applied:?}"
        );

        better_gate.release();
        let selected =
            match tokio::time::timeout(Duration::from_secs(10), activated_rx.recv()).await {
                Ok(Some(selected)) => selected,
                Ok(None) => {
                    panic!("activation channel closed before usable candidate; applied={applied:?}")
                }
                Err(_) => {
                    panic!("timed out waiting for usable candidate activation; applied={applied:?}")
                }
            };
        assert_eq!(selected, ("claude".to_string(), "better".to_string()));

        let (snapshots, auto_lines) =
            tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .expect("fill task should finish after better activation")
                .unwrap();
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
