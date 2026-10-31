//! subswapd:subswap 后台守护进程。
//!
//! 职责(M4):
//! 1. 每 `DAEMON_POLL_INTERVAL_MS` 跑一轮:对每个 Provider 拉 quota → 跑 AutoSwapPolicy →
//!    必要时执行 swap → 写审计日志。
//! 2. Claude token 后台保活:扫所有 Claude 账号,临近过期且有 refreshToken 的触发刷新,
//!    只回写 keyring(不动 ~/.claude/)。
//! 3. 冷却 / flap 检测全在内存:进程重启即重置(刻意为之,简化故障语义)。
//! 4. SIGTERM / SIGINT 收尾退出(写完当前一轮就停)。
//!
//! 不变量(对齐 docs/design/AUTO_SWAP_DESIGN.md):
//! - 手动 swap 永远不依赖本进程(本进程挂了不影响 `subswap swap`)。
//! - quota 查询失败 → Degraded,不猜测,不补打请求。
//! - 不通过高频轮询「探测」429。

#[path = "state.rs"]
mod state;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use subswap_core::quota_schedule::{
    candidate_refresh_plan, fetch_state_for_observation, next_due_at_with_deadline, next_sleep,
    parse_cached_reset_seeds, refresh_class, reset_candidates_for_account, ObservedAccount,
    ResetTracker, ScheduleConfig,
};
use subswap_core::{
    auto_decide, collect_quota, collect_quota_at, is_authentication_failure, paths::AppPaths,
    quota_collection, request_metrics, settings, AccountId, AccountRegistry, AccountWithQuotas,
    AuditEvent, AuditLog, FileStore, KeyringStore, PolicyConfig, PolicyDecision, Provider,
    ProviderRegistry, ProviderSnapshot, Quota, QuotaCache, QuotaFetchState, QuotaPoolSemantics,
    QuotaReading,
};
use subswap_provider_antigravity::AntigravityProvider;
use subswap_provider_claude::ClaudeProvider;
use subswap_provider_codex::CodexProvider;
use subswap_provider_commandcode::CommandcodeProvider;
use subswap_provider_cursor::CursorProvider;
use subswap_provider_kimi::KimiProvider;
use subswap_provider_opencode::{OpencodeApiKeyProvider, OpencodeProvider};
use tokio::signal::unix::{signal, SignalKind};

use state::DaemonState;

/// Provider 列表读取失败的告警节流：同一 Provider 至多每 30 分钟告警一次。
///
/// 原生客户端没装/没登录时 `list_accounts` 可能每轮都返回同类错误；每秒/每分钟重复
/// 一行 warn 只会淹没日志，并不能提供新信息。
const PROVIDER_WARN_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// 跨轮次保留的调度状态：已知 reset 去重、告警节流。
struct AdaptiveState {
    reset_tracker: ResetTracker,
    provider_warnings: HashMap<String, Instant>,
}

impl AdaptiveState {
    fn new() -> Self {
        Self {
            reset_tracker: ResetTracker::new(),
            provider_warnings: HashMap::new(),
        }
    }

    fn should_warn(&mut self, provider: &str) -> bool {
        let now = Instant::now();
        match self.provider_warnings.get(provider) {
            Some(last) if now.duration_since(*last) < PROVIDER_WARN_INTERVAL => false,
            _ => {
                self.provider_warnings.insert(provider.to_string(), now);
                true
            }
        }
    }

    fn clear_warning(&mut self, provider: &str) {
        self.provider_warnings.remove(provider);
    }
}

/// 一轮采集后的调度结论，供下一轮睡眠计算。
#[derive(Debug, Default)]
struct CycleOutcome {
    next_due: Option<DateTime<Utc>>,
    next_reset: Option<DateTime<Utc>>,
}

pub async fn run() -> Result<()> {
    let log_level = std::env::var("SUBSWAPD_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_new(&log_level)
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let paths = AppPaths::resolve().context("resolve app paths")?;

    // PID 文件 + 文件锁:唯一存活实例。锁获取失败 → 已有实例运行,本进程退出。
    let pid_path = paths.daemon_pid_file();
    let pid_file = open_pid_lock(&pid_path)?;
    if pid_file.try_lock_exclusive().is_err() {
        tracing::info!(
            pid_file = %pid_path.display(),
            "another subswapd already holds the lock; exiting"
        );
        return Ok(());
    }
    write_pid(&pid_path, process::id())?;
    tracing::info!(pid = process::id(), "subswapd started");

    // 凭证后端：明文文件 + 旧钥匙串懒迁移（与 CLI 一致，避免 macOS 钥匙串授权框）。
    let store = Arc::new(FileStore::with_legacy_keyring(
        paths.credentials_file(),
        KeyringStore::new(),
    ));
    let registry = Arc::new(AccountRegistry::from_default_paths()?);
    subswap_provider_opencode::migrate_legacy_api_keys(store.as_ref(), &registry)?;
    let audit = AuditLog::from_default_paths()?;

    let claude = Arc::new(ClaudeProvider::new(store.clone(), registry.clone()));
    let codex = Arc::new(subswap_provider_codex::new(store.clone(), registry.clone()));
    let kimi = Arc::new(subswap_provider_kimi::new(store.clone(), registry.clone()));
    let cursor = Arc::new(CursorProvider::new(store.clone(), registry.clone())?);
    let opencode = Arc::new(subswap_provider_opencode::new(
        store.clone(),
        registry.clone(),
    ));
    let opencode_api_key = Arc::new(OpencodeApiKeyProvider::new(
        opencode.go_engine(),
        registry.clone(),
    ));
    let commandcode = Arc::new(subswap_provider_commandcode::new(
        store.clone(),
        registry.clone(),
    ));
    let antigravity = Arc::new(AntigravityProvider::new(store.clone(), registry.clone()));
    let mut providers = ProviderRegistry::new();
    providers.register(claude.clone());
    providers.register(codex.clone());
    providers.register(kimi.clone());
    providers.register(cursor.clone());
    providers.register(opencode.clone());
    providers.register(opencode_api_key.clone());
    providers.register(commandcode.clone());
    providers.register(antigravity.clone());

    let mut state = DaemonState::new();
    let mut adaptive = AdaptiveState::new();
    let cache_path = paths.quota_cache_file();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        // 1. 每轮开头热加载配置；解析失败则沿用上次成功值 + warn。
        if let Err(e) = settings::reload_from_file() {
            tracing::warn!(err = %e, "reload config failed; keeping previous values");
        }
        let snapshot_settings = settings::current();

        // 2. 跑一轮调度。
        let policy = PolicyConfig {
            enabled: snapshot_settings.auto_swap.enabled,
            threshold: snapshot_settings.auto_swap.threshold,
            allow_unknown: false,
            settle_grace_ms: snapshot_settings.auto_swap.settle_grace_ms,
            manual_hold_ms: snapshot_settings.auto_swap.manual_hold_ms,
        };
        let outcome = match run_cycle(
            &providers,
            &registry,
            &claude,
            &codex,
            &kimi,
            &cursor,
            &opencode,
            &opencode_api_key,
            &commandcode,
            &antigravity,
            &audit,
            &mut state,
            &mut adaptive,
            &policy,
            &paths.data_dir,
            &cache_path,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(err = %e, "daemon cycle failed; will retry next interval");
                CycleOutcome::default()
            }
        };

        // 3. 既有活跃 / 空闲节奏 + 自适应到期 + 已知 reset 唤醒取最小值。
        //    空闲时不被普通自适应到期提前唤醒（保持「用户没在用就少打请求」），
        //    但已知 reset 仍可把睡眠缩短到 reset + 配置延迟。
        let active = user_is_active(&providers, &snapshot_settings.daemon);
        let existing_wake = decide_next_interval(active, &snapshot_settings.daemon);
        let overdue_retry = Duration::from_millis(snapshot_settings.daemon.poll_interval_ms)
            .max(Duration::from_secs(1));
        let next_due = active.then_some(outcome.next_due).flatten();
        let poll = next_sleep(
            Utc::now(),
            existing_wake,
            next_due,
            outcome.next_reset,
            overdue_retry,
        );
        tracing::debug!(
            interval_ms = poll.as_millis() as u64,
            user_active = active,
            next_due = ?next_due,
            next_reset = ?outcome.next_reset,
            "sleeping until next cycle"
        );

        tokio::select! {
            _ = tokio::time::sleep(poll) => {}
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received; shutting down");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received; shutting down");
                break;
            }
        }
    }

    // best-effort 清理 PID 文件(锁会随进程退出自动释放)。
    let _ = std::fs::remove_file(&pid_path);
    Ok(())
}

/// 一轮调度:拉 quota → 决策 → swap → token 保活。任一 Provider 失败不影响其他。
// 参数数量是单次调度轮次编排的固有需要(各 provider 句柄 + 状态 + 配置),非公开 API,
// 拆结构体收益不大,直接放行该 lint。
#[allow(clippy::too_many_arguments)]
async fn run_cycle(
    providers: &ProviderRegistry,
    account_registry: &AccountRegistry,
    claude: &Arc<ClaudeProvider>,
    codex: &Arc<CodexProvider>,
    kimi: &Arc<KimiProvider>,
    cursor: &Arc<CursorProvider>,
    opencode: &Arc<OpencodeProvider>,
    opencode_api_key: &Arc<OpencodeApiKeyProvider>,
    commandcode: &Arc<CommandcodeProvider>,
    antigravity: &Arc<AntigravityProvider>,
    audit: &AuditLog,
    state: &mut DaemonState,
    adaptive: &mut AdaptiveState,
    policy: &PolicyConfig,
    data_dir: &std::path::Path,
    cache_path: &Path,
) -> Result<CycleOutcome> {
    // (0) 持续回灌:把 active 账号的 live 凭证抓回 store(只读 live、只写 store、不刷新)。
    // 填补「在 Claude Code 内直接切走、未经 subswap swap」的捕获缺口,使该账号日后变 parked 时
    // store 里仍是活 token,parked 自刷不依赖再开 Claude Code。best-effort,失败仅 debug。
    if let Err(e) = claude.reconcile_active_from_live().await {
        tracing::debug!(err = %e, "claude live-credential reconcile skipped");
    }
    // Codex/Kimi 走共享文件型引擎,reconcile 是同步阻塞 IO,包进 spawn_blocking 调用
    // (不用 block_in_place:该引擎的 activate 同样避开它,原因是调用方可能跑在
    // current-thread runtime 上会直接 panic,此处沿用同一取舍)。
    reconcile_file_blob_provider(codex, "codex").await;
    reconcile_file_blob_provider(kimi, "kimi").await;
    let api_key = opencode_api_key.clone();
    match tokio::task::spawn_blocking(move || api_key.sync_accounts(None)).await {
        Ok(Err(e)) => tracing::debug!(err = %e, "OpenCode API key sync skipped"),
        Err(e) => tracing::debug!(err = %e, "OpenCode API key sync task failed"),
        Ok(Ok(_)) => {}
    }
    let console = opencode.clone();
    let console_sync_ready =
        match tokio::task::spawn_blocking(move || console.sync_console_accounts(None)).await {
            Ok(Ok(Some(_))) => true,
            Ok(Ok(None)) => false,
            Ok(Err(e)) => {
                warn_sync_failure(
                    adaptive,
                    account_registry,
                    "opencode",
                    "opencode-console-sync",
                    &e,
                )
                .await;
                false
            }
            Err(e) => {
                warn_sync_failure(
                    adaptive,
                    account_registry,
                    "opencode",
                    "opencode-console-sync",
                    &e,
                )
                .await;
                false
            }
        };
    reconcile_file_blob_provider(commandcode, "commandcode").await;
    // Antigravity capture-on-arrival（只 live→store，不刷新不写 live）。
    {
        let a = antigravity.clone();
        match tokio::task::spawn_blocking(move || a.reconcile_active_from_live()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::debug!(err = %e, provider = "antigravity", "live-credential reconcile skipped")
            }
            Err(e) => {
                tracing::debug!(err = %e, provider = "antigravity", "live-credential reconcile join failed")
            }
        }
    }
    // Cursor 只有一份 live 凭证。此处必须在读额度前先将外部新登录账号入池；失败时
    // 本轮禁止覆盖 Cursor，避免旧账号池把用户刚完成的原生登录写回去。
    let cursor_sync_ready = match cursor.reconcile_active_from_live().await {
        Ok(()) => {
            adaptive.clear_warning("cursor-sync");
            true
        }
        Err(e) => {
            warn_sync_failure(adaptive, account_registry, "cursor", "cursor-sync", &e).await;
            false
        }
    };

    // (a) 收集快照：只刷新到期 / reset 唤醒 / 近阈值候选，其余并回持久缓存。
    let all_settings = settings::current();
    let (snapshots, outcome) = collect_snapshots(
        providers,
        account_registry,
        adaptive,
        cache_path,
        &all_settings,
        policy.threshold,
    )
    .await;

    // (b) per-provider 跑决策 + 执行。
    for snap in &snapshots {
        if snap.accounts.is_empty() {
            continue;
        }

        if snap.provider == "cursor" && !cursor_sync_ready {
            continue;
        }
        if snap.provider == "opencode" && !console_sync_ready {
            continue;
        }

        // Degraded 期内跳过该 Provider 的 swap(但 token 保活仍要做)。
        if state.is_degraded(&snap.provider) {
            tracing::debug!(provider = %snap.provider, "provider in degraded window; skip");
            continue;
        }
        match auto_decide(snap, policy) {
            PolicyDecision::Swap { from, to, .. } => {
                // quota 查询期间用户可能刚在 Cursor 原生客户端完成新登录。切换前再同步一次：
                // 若账号变了，下面的 active 复核会丢弃陈旧决策，绝不覆盖新凭证。
                if snap.provider == "cursor" {
                    if let Err(e) = cursor.reconcile_active_from_live().await {
                        tracing::warn!(err = %e, "skip stale Cursor auto swap because live login did not sync");
                        continue;
                    }
                }

                // 冷却:刚被切走 / 切到的账号短期不再选回。
                if state.in_cooldown(&snap.provider, &to) {
                    tracing::debug!(
                        provider = %snap.provider,
                        target = %to,
                        "candidate in cooldown; skip this cycle"
                    );
                    continue;
                }

                let provider = match providers.get(&snap.provider) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(err = %e, provider = %snap.provider, "lookup provider failed");
                        continue;
                    }
                };
                let current_accounts = match provider.list_accounts().await {
                    Ok(accounts) => accounts,
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            provider = %snap.provider,
                            "recheck active account failed; skip stale auto swap"
                        );
                        continue;
                    }
                };
                if !auto_swap_still_allowed(&current_accounts, from.as_ref()) {
                    tracing::info!(
                        provider = %snap.provider,
                        "active account changed during quota query; skip stale auto swap"
                    );
                    continue;
                }
                match provider.activate(&to).await {
                    Ok(()) => {
                        audit.append(AuditEvent::ok(
                            "auto_swap",
                            &snap.provider,
                            Some(to.0.as_str()),
                        ));
                        state.record_swap(&snap.provider, &to);
                        tracing::info!(
                            provider = %snap.provider,
                            target = %to,
                            "auto swap done"
                        );

                        // flap 检测:5min 内 ≥ MAX_FLAP_PER_5MIN 次 → Degraded 30min。
                        if state.detect_flap(&snap.provider) {
                            state.mark_degraded(&snap.provider);
                            audit.append(AuditEvent::err(
                                "auto_degraded",
                                &snap.provider,
                                None,
                                "flap detected",
                            ));
                            tracing::warn!(
                                provider = %snap.provider,
                                "flap threshold hit; entering degraded window"
                            );
                        }
                    }
                    Err(e) => {
                        audit.append(AuditEvent::err(
                            "auto_swap",
                            &snap.provider,
                            Some(to.0.as_str()),
                            &e.to_string(),
                        ));
                        tracing::warn!(
                            provider = %snap.provider,
                            target = %to,
                            err = %e,
                            "auto swap failed"
                        );
                    }
                }
            }
            PolicyDecision::Degraded { reason } => {
                tracing::debug!(provider = %snap.provider, reason = %reason, "degraded");
            }
            // 手动保持走 info（用户显式选择值得在日志里露出一行），其余 NoOp 仍 debug。
            PolicyDecision::NoOp { reason } if reason.contains("manually selected") => {
                tracing::info!(provider = %snap.provider, reason = %reason, "auto swap held");
            }
            PolicyDecision::NoOp { .. } => {}
        }
    }

    // (c) Claude token 后台保活:对所有 Claude 账号检查 expires_at。
    keep_claude_tokens_alive(claude, data_dir, adaptive).await;
    keep_antigravity_tokens_alive(antigravity, data_dir, adaptive).await;
    Ok(outcome)
}

/// 同步失败告警：有对应账号时最多每 30 分钟一行；本地完全没有该 provider 的账号时
/// 只留 debug，避免「没装/没登录」的客户端每分钟刷一条 warn。
async fn warn_sync_failure<E: std::fmt::Display>(
    adaptive: &mut AdaptiveState,
    account_registry: &AccountRegistry,
    provider: &str,
    warning_key: &str,
    error: &E,
) {
    let has_accounts = provider_has_accounts(account_registry.path(), provider).await;
    if !has_accounts {
        tracing::debug!(
            err = %error,
            provider,
            "live credential sync skipped; no local accounts to protect"
        );
        return;
    }
    if adaptive.should_warn(warning_key) {
        tracing::warn!(
            err = %error,
            provider,
            "live credential sync failed; skipping automatic swap for this provider"
        );
    }
}

/// 本地注册表里是否有该 Provider 的账号；文件 IO 放阻塞线程。
async fn provider_has_accounts(path: &Path, provider: &str) -> bool {
    let path = path.to_path_buf();
    let provider = provider.to_string();
    tokio::task::spawn_blocking(move || {
        AccountRegistry::new(path)
            .list_by_provider(&provider)
            .map(|accounts| !accounts.is_empty())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

/// 文件型共享引擎（Codex/Kimi）的 capture-on-arrival：同步阻塞 IO 包进 spawn_blocking,
/// best-effort,失败仅 debug 日志,不影响本轮调度。
async fn reconcile_file_blob_provider<A>(
    provider: &Arc<subswap_provider_common::FileBlobProvider<A>>,
    label: &'static str,
) where
    A: subswap_provider_common::FileBlobRuntime,
{
    let provider = provider.clone();
    match tokio::task::spawn_blocking(move || provider.reconcile_active_from_live()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::debug!(err = %e, provider = %label, "live-credential reconcile skipped")
        }
        Err(e) => {
            tracing::debug!(err = %e, provider = %label, "live-credential reconcile join failed")
        }
    }
}

/// quota 查询期间用户可能手动切换账号；自动切换执行前必须丢弃过期决策。
fn auto_swap_still_allowed(
    current_accounts: &[subswap_core::Account],
    expected_from: Option<&subswap_core::AccountId>,
) -> bool {
    let current = current_accounts.iter().find(|account| account.active);
    if current.is_some_and(|account| account.manual_only()) {
        return false;
    }
    current.map(|account| &account.id) == expected_from
}

/// 读取配额缓存；文件 IO 放到阻塞线程，避免在 async 调度轮次里直接读盘。
async fn load_quota_cache(path: &Path) -> QuotaCache {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || QuotaCache::load(&path))
        .await
        .unwrap_or_default()
}

/// 从优先升级前已存在的 `quota_cache.json` 读取原始观测，作为 reset 唤醒种子。
///
/// 只读、忽略损坏内容；即使协调器记录尚未生成，也能为已知窗口排定到期刷新。
async fn load_cached_reset_seeds(path: &Path) -> HashMap<(String, String), Vec<Quota>> {
    let path = path.to_path_buf();
    let seeds = tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(&path)
            .ok()
            .map(|raw| parse_cached_reset_seeds(&raw))
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    seeds
        .into_iter()
        .map(|seed| ((seed.provider, seed.account_id.0), seed.quotas))
        .collect()
}

/// 一个 Provider 的本轮采集清单与持久状态。
struct Inventory {
    provider: Arc<dyn Provider>,
    provider_id: String,
    semantics: QuotaPoolSemantics,
    /// 账号 + 最近一次持久成功额度 + 原始观测时间。
    observed: Vec<ObservedAccount>,
    /// 协调器落盘的 `next_attempt_at`（请求地板 / Retry-After）。
    deadlines: HashMap<String, Option<DateTime<Utc>>>,
    /// 协调记录中的原始成功额度；用于 reset 唤醒。缓存视图会过滤已到期的窗口，
    /// 只看缓存会恰好丢掉「刚刚到 reset」的窗口，导致无法显式刷新受影响账号。
    reset_quotas: HashMap<String, Vec<Quota>>,
    /// 协调记录不可读：fail closed，本轮不采这个账号。
    coordination_unreadable: HashSet<String>,
}

/// 自适应采集并生成 Provider 快照。
///
/// 只请求三类账号：普通到期（激活 3m / 停放与耗尽 15m）、已知 reset 到期的受影响账号、
/// 以及激活账号接近阈值时最多两个缺少新鲜确认的候选。其余账号从持久缓存并回。
/// 失败/旧数据的区分在这里保留：超过 `decision_max_age` 只会是 `Stale`，不会变成 Ready。
async fn collect_snapshots(
    providers: &ProviderRegistry,
    account_registry: &AccountRegistry,
    adaptive: &mut AdaptiveState,
    cache_path: &Path,
    settings: &settings::Settings,
    threshold: f64,
) -> (Vec<ProviderSnapshot>, CycleOutcome) {
    let schedule_cfg = ScheduleConfig::from_settings(&settings.quota);
    let now = Utc::now();
    let cache = load_quota_cache(cache_path).await;
    let cached_reset_seeds = load_cached_reset_seeds(cache_path).await;
    let mut inventories = Vec::new();

    for provider in providers.all() {
        let provider_id = provider.id().to_string();
        let accounts = match provider.list_accounts().await {
            Ok(accounts) => {
                adaptive.clear_warning(&provider_id);
                accounts
            }
            Err(e) => {
                // 原生客户端缺失/未登录时可能每轮都失败；节流到 30 分钟一行，
                // 不再让同一条错误每分钟刷屏。本地没有任何对应账号时连这一行也省掉。
                let has_accounts =
                    provider_has_accounts(account_registry.path(), &provider_id).await;
                if has_accounts && adaptive.should_warn(&provider_id) {
                    tracing::warn!(
                        err = %e,
                        provider = %provider_id,
                        "list_accounts failed; retry on a later cycle"
                    );
                }
                continue;
            }
        };
        let mut observed = Vec::with_capacity(accounts.len());
        let mut deadlines = HashMap::with_capacity(accounts.len());
        let mut reset_quotas = HashMap::new();
        let mut coordination_unreadable = HashSet::new();
        for account in accounts {
            let entry = cache.get(&provider_id, &account.id.0);
            let quotas = entry.as_ref().map(|e| e.quotas.clone()).unwrap_or_default();
            let mut observed_at = entry.as_ref().map(|e| e.cached_at);
            let account_key = (provider_id.clone(), account.id.0.clone());
            match quota_collection::read_status_async(cache_path, &provider_id, &account.id.0).await
            {
                Ok(Some(status)) => {
                    // 成功但额度为空（例如自定义 API 账号）不会进入缓存条目；仍要沿用协调器
                    // 记录的观测时间，否则会被当成「从未观测」而每 90 秒重采一次。
                    if observed_at.is_none()
                        && matches!(status.state, quota_collection::CollectionState::Success)
                    {
                        observed_at = status.observed_at;
                    }
                    deadlines.insert(account.id.0.clone(), status.next_attempt_at);
                    if !status.quotas.is_empty() {
                        reset_quotas.insert(account.id.0.clone(), status.quotas);
                    }
                }
                Ok(None) => {
                    deadlines.insert(account.id.0.clone(), None);
                }
                Err(e) => {
                    tracing::debug!(
                        err = %e,
                        provider = %provider_id,
                        account = %account.id,
                        "quota collection state unreadable; skip this account"
                    );
                    deadlines.insert(account.id.0.clone(), None);
                    coordination_unreadable.insert(account.id.0.clone());
                }
            }
            // 协调器尚无记录（例如刚升级）时，用缓存文件里的原始观测作为 reset 种子。
            if !reset_quotas.contains_key(&account.id.0) {
                if let Some(seed) = cached_reset_seeds.get(&account_key) {
                    reset_quotas.insert(account.id.0.clone(), seed.clone());
                }
            }
            observed.push(ObservedAccount::new(account, quotas, observed_at));
        }
        inventories.push(Inventory {
            semantics: provider.quota_pool_semantics(),
            provider,
            provider_id,
            observed,
            deadlines,
            reset_quotas,
            coordination_unreadable,
        });
    }

    // 已知 gating 窗口 reset：枚举每个窗口的到期时间（健康账号同样刷新），
    // 同一账号同一时刻去重；多窗口的不同到期时间会依次调度。
    let mut all_resets = Vec::new();
    for inventory in &inventories {
        for account in &inventory.observed {
            let id = account.account.id.0.as_str();
            // 优先用协调记录里的原始成功额度；没有记录时用升级前缓存观测作种子。
            // 缓存视图会过滤刚刚到期的窗口，不能只依赖 `cache.get`。
            let quotas = inventory
                .reset_quotas
                .get(id)
                .map(Vec::as_slice)
                .unwrap_or(account.quotas.as_slice());
            let deadline = inventory.deadlines.get(id).copied().flatten();
            all_resets.extend(reset_candidates_for_account(
                &inventory.provider_id,
                &account.account.id,
                quotas,
                deadline,
            ));
        }
    }
    let due_wake = adaptive
        .reset_tracker
        .due_wake(now, &all_resets, schedule_cfg.reset_delay);
    let mut forced: HashSet<(String, String)> = HashSet::new();
    if let Some(wake) = &due_wake {
        adaptive.reset_tracker.mark_handled(wake);
        for target in &wake.targets {
            forced.insert((target.provider.clone(), target.account_id.0.clone()));
        }
        tracing::debug!(
            accounts = wake.targets.len(),
            "quota reset window reached; refreshing affected accounts"
        );
    }

    // 组装本轮采集清单：普通到期 ∪ reset 唤醒 ∪ 近阈值候选（有上限）。
    let mut jobs_by_provider: HashMap<String, Vec<AccountId>> = HashMap::new();
    let mut next_due: Option<DateTime<Utc>> = None;
    let mut deferred_events: Vec<(String, String)> = Vec::new();
    for inventory in &inventories {
        for account in &inventory.observed {
            let id = account.account.id.0.clone();
            let class = refresh_class(account, inventory.semantics);
            let deadline = inventory.deadlines.get(&id).copied().flatten();
            let due_at = next_due_at_with_deadline(
                &inventory.provider_id,
                &id,
                class,
                account.observed_at,
                deadline,
                &schedule_cfg,
                now,
            );
            next_due = Some(next_due.map_or(due_at, |current| current.min(due_at)));
            let is_forced = forced.contains(&(inventory.provider_id.clone(), id.clone()));
            let is_due = now >= due_at;
            if !is_due && !is_forced {
                continue;
            }
            if inventory.coordination_unreadable.contains(&id) {
                continue;
            }
            if deadline.is_some_and(|deadline| deadline > now) {
                // 候选提前确认也要服从请求地板/服务端 Retry-After。
                deferred_events.push((inventory.provider_id.clone(), id));
                continue;
            }
            add_job(
                &mut jobs_by_provider,
                &inventory.provider_id,
                &account.account.id,
            );
        }
    }

    for inventory in &inventories {
        let plan = candidate_refresh_plan(&inventory.observed, threshold, &schedule_cfg, now);
        for id in plan.selected {
            let deadline = inventory.deadlines.get(&id.0).copied().flatten();
            if inventory.coordination_unreadable.contains(&id.0) {
                continue;
            }
            if deadline.is_some_and(|deadline| deadline > now) {
                deferred_events.push((inventory.provider_id.clone(), id.0.clone()));
                if let Some(deadline) = deadline {
                    next_due = Some(next_due.map_or(deadline, |current| current.min(deadline)));
                }
                continue;
            }
            add_job(&mut jobs_by_provider, &inventory.provider_id, &id);
        }
        // 超过上限的候选不额外请求，但记录一次 deferred，说明是调度上限而非查询失败。
        for id in plan.deferred {
            deferred_events.push((inventory.provider_id.clone(), id.0));
        }
    }

    for (provider, account) in &deferred_events {
        request_metrics::record(provider, account, "deferred", None).await;
    }

    // 并发采集：全局上限 + 同 Provider 串行（每个 Provider 一个任务顺序处理自己的队列）。
    let production_cache = AppPaths::resolve()
        .ok()
        .is_some_and(|paths| paths.quota_cache_file() == cache_path);
    let permits = Arc::new(tokio::sync::Semaphore::new(
        schedule_cfg.max_concurrent_fetches.max(1),
    ));
    let mut outcomes: HashMap<(String, String), std::result::Result<QuotaReading, String>> =
        HashMap::new();
    let mut set = tokio::task::JoinSet::new();
    for inventory in &inventories {
        let Some(job_ids) = jobs_by_provider.remove(&inventory.provider_id) else {
            continue;
        };
        let provider = inventory.provider.clone();
        let provider_id = inventory.provider_id.clone();
        let semaphore = permits.clone();
        let cache_path = cache_path.to_path_buf();
        let settings = settings.clone();
        set.spawn(async move {
            let mut collected = Vec::with_capacity(job_ids.len());
            for id in job_ids {
                let Ok(permit) = semaphore.clone().acquire_owned().await else {
                    break;
                };
                let result = if production_cache {
                    collect_quota(provider.as_ref(), &id).await
                } else {
                    collect_quota_at(provider.as_ref(), &id, &cache_path, &settings).await
                };
                drop(permit);
                collected.push((provider_id.clone(), id, result.map_err(|e| e.to_string())));
            }
            collected
        });
    }
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(collected) => {
                for (provider, id, result) in collected {
                    outcomes.insert((provider, id.0), result);
                }
            }
            Err(e) => tracing::warn!(err = %e, "quota collection task failed"),
        }
    }

    // 协调器已持久化本轮真实结果；重读缓存后逐账号并回，保留 stale/error 区分。
    let cache = load_quota_cache(cache_path).await;
    let now = Utc::now();
    let mut snapshots = Vec::with_capacity(inventories.len());
    let mut final_observed = Vec::with_capacity(inventories.len());
    for inventory in inventories {
        let mut accounts_out = Vec::with_capacity(inventory.observed.len());
        let mut observed_out = Vec::with_capacity(inventory.observed.len());
        for account in &inventory.observed {
            let id = account.account.id.0.clone();
            let outcome = outcomes.remove(&(inventory.provider_id.clone(), id.clone()));
            let (quotas, observed_at, fetch_state) = merge_collected_state(
                outcome,
                &cache,
                &inventory.provider_id,
                &id,
                &schedule_cfg,
                now,
            );
            accounts_out.push(AccountWithQuotas {
                account: account.account.clone(),
                quotas: quotas.clone(),
                fetch_state,
            });
            observed_out.push(ObservedAccount::new(
                account.account.clone(),
                quotas,
                observed_at,
            ));
        }
        snapshots.push(ProviderSnapshot {
            provider: inventory.provider_id.clone(),
            accounts: accounts_out,
            pool_semantics: inventory.semantics,
        });
        final_observed.push((inventory.provider_id, inventory.semantics, observed_out));
    }

    // 用采集后的真实数据重算下一到期与下一次 reset 唤醒。
    let mut next_due: Option<DateTime<Utc>> = None;
    let mut all_resets = Vec::new();
    for (provider_id, semantics, observed) in &final_observed {
        for account in observed {
            let status =
                quota_collection::read_status_async(cache_path, provider_id, &account.account.id.0)
                    .await
                    .ok()
                    .flatten();
            let deadline = status.as_ref().and_then(|status| status.next_attempt_at);
            let class = refresh_class(account, *semantics);
            let due_at = next_due_at_with_deadline(
                provider_id,
                &account.account.id.0,
                class,
                account.observed_at,
                deadline,
                &schedule_cfg,
                now,
            );
            next_due = Some(next_due.map_or(due_at, |current| current.min(due_at)));

            let quotas = status
                .as_ref()
                .filter(|status| !status.quotas.is_empty())
                .map(|status| status.quotas.as_slice())
                .unwrap_or_else(|| {
                    cached_reset_seeds
                        .get(&(provider_id.clone(), account.account.id.0.clone()))
                        .map(Vec::as_slice)
                        .unwrap_or(account.quotas.as_slice())
                });
            all_resets.extend(reset_candidates_for_account(
                provider_id,
                &account.account.id,
                quotas,
                deadline,
            ));
        }
    }
    adaptive.reset_tracker.prune(&all_resets);
    let next_reset = adaptive
        .reset_tracker
        .next_wake(now, &all_resets, schedule_cfg.reset_delay)
        .map(|wake| wake.at);

    (
        snapshots,
        CycleOutcome {
            next_due,
            next_reset,
        },
    )
}

fn add_job(jobs: &mut HashMap<String, Vec<AccountId>>, provider: &str, id: &AccountId) {
    let entries = jobs.entry(provider.to_string()).or_default();
    if !entries.iter().any(|queued| queued == id) {
        entries.push(id.clone());
    }
}

/// 将单个账号的采集结果（或无结果）合并进快照。
///
/// 失败已由协调器记账，这里不再 `record_failure`；缓存只读并回。401/403 这类确定
/// 鉴权失败清掉旧数字；网络/429 且存在旧成功观测时保留 `Stale { cached_at, error }`。
fn merge_collected_state(
    outcome: Option<std::result::Result<QuotaReading, String>>,
    cache: &QuotaCache,
    provider: &str,
    account_id: &str,
    cfg: &ScheduleConfig,
    now: DateTime<Utc>,
) -> (Vec<Quota>, Option<DateTime<Utc>>, QuotaFetchState) {
    match outcome {
        Some(Ok(reading)) => {
            // 合并结果可能来自稍早的并发成功；若缓存里存在更晚的失败事件，展示必须
            // 反映最近一次真实事件，不能把旧成功盖成 Ready。
            if let Some(failure) = cache
                .failure(provider, account_id)
                .filter(|failure| failure.failed_at > reading.observed_at)
            {
                if is_authentication_failure(&failure.error) {
                    return (
                        Vec::new(),
                        Some(reading.observed_at),
                        QuotaFetchState::Failed(failure.error.clone()),
                    );
                }
                return (
                    reading.quotas,
                    Some(reading.observed_at),
                    QuotaFetchState::Stale {
                        cached_at: reading.observed_at,
                        error: failure.error.clone(),
                    },
                );
            }
            let state = fetch_state_for_observation(Some(reading.observed_at), now, cfg);
            (reading.quotas, Some(reading.observed_at), state)
        }
        Some(Err(error)) => {
            if is_authentication_failure(&error) {
                return (Vec::new(), None, QuotaFetchState::Failed(error));
            }
            match cache.get(provider, account_id) {
                Some(entry) => (
                    entry.quotas,
                    Some(entry.cached_at),
                    QuotaFetchState::Stale {
                        cached_at: entry.cached_at,
                        error,
                    },
                ),
                None => (Vec::new(), None, QuotaFetchState::Failed(error)),
            }
        }
        None => match cache.get(provider, account_id) {
            Some(entry) => {
                if let Some(failure) = cache.failure(provider, account_id) {
                    if is_authentication_failure(&failure.error) {
                        return (
                            Vec::new(),
                            None,
                            QuotaFetchState::Failed(failure.error.clone()),
                        );
                    }
                    return (
                        entry.quotas,
                        Some(entry.cached_at),
                        QuotaFetchState::Stale {
                            cached_at: entry.cached_at,
                            error: failure.error.clone(),
                        },
                    );
                }
                let state = fetch_state_for_observation(Some(entry.cached_at), now, cfg);
                (entry.quotas, Some(entry.cached_at), state)
            }
            None => {
                let state = match cache.failure(provider, account_id) {
                    Some(failure) => QuotaFetchState::Failed(failure.error.clone()),
                    None => QuotaFetchState::Loading,
                };
                (Vec::new(), None, state)
            }
        },
    }
}

/// 扫 Claude 账号,临近过期的触发 refresh。任一账号失败仅 warn。
///
/// **跳过被隔离会话借走的账号**：该账号的 token 此刻由隔离环境里的 Claude Code 唯一轮换，
/// daemon 再去刷会与之冲突导致 refresh token 被作废（同 active 账号守卫的道理，见
/// docs/design/ACCOUNT_ISOLATION_DESIGN.md §5）。
async fn keep_claude_tokens_alive(
    claude: &Arc<ClaudeProvider>,
    data_dir: &std::path::Path,
    adaptive: &mut AdaptiveState,
) {
    let accounts = match claude.list_accounts().await {
        Ok(a) => a,
        Err(e) => {
            if adaptive.should_warn("claude-keepalive") {
                tracing::warn!(err = %e, "claude list_accounts failed during keepalive");
            }
            return;
        }
    };
    for account in accounts {
        if account_checked_out(
            data_dir.to_path_buf(),
            "claude".to_string(),
            account.id.0.clone(),
        )
        .await
        {
            tracing::debug!(account = %account.id, "skip keepalive: checked out by isolated session");
            continue;
        }
        match claude.refresh_if_near_expiry(&account.id).await {
            Ok(true) => {
                tracing::info!(account = %account.id, "claude token refreshed");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    account = %account.id,
                    err = %e,
                    "claude token refresh failed"
                );
            }
        }
    }
}

/// 扫 Antigravity 账号，临近过期的 parked 账号触发 refresh（active 由 agy 自己轮换）。
async fn keep_antigravity_tokens_alive(
    antigravity: &Arc<AntigravityProvider>,
    data_dir: &std::path::Path,
    adaptive: &mut AdaptiveState,
) {
    let accounts = match antigravity.list_accounts().await {
        Ok(a) => a,
        Err(e) => {
            if adaptive.should_warn("antigravity-keepalive") {
                tracing::warn!(err = %e, "antigravity list_accounts failed during keepalive");
            }
            return;
        }
    };
    for account in accounts {
        if account_checked_out(
            data_dir.to_path_buf(),
            "antigravity".to_string(),
            account.id.0.clone(),
        )
        .await
        {
            tracing::debug!(account = %account.id, "skip keepalive: checked out by isolated session");
            continue;
        }
        match antigravity.refresh_if_near_expiry(&account.id).await {
            Ok(true) => {
                tracing::info!(account = %account.id, "antigravity token refreshed");
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(
                    account = %account.id,
                    err = %e,
                    "antigravity token refresh failed"
                );
            }
        }
    }
}

async fn account_checked_out(data_dir: PathBuf, provider: String, id: String) -> bool {
    tokio::task::spawn_blocking(move || {
        subswap_core::checkout::is_checked_out(&data_dir, &provider, &id)
    })
    .await
    .unwrap_or(false)
}

fn open_pid_lock(path: &PathBuf) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open pid file {}", path.display()))
}

fn write_pid(path: &PathBuf, pid: u32) -> Result<()> {
    std::fs::write(path, pid.to_string().as_bytes())
        .with_context(|| format!("write pid to {}", path.display()))
}

/// 根据「最近一次客户端活动」判断用户是否活跃。
///
/// 活动信号：所有 provider `client_targets().probe_path` 中最近一次 mtime。
/// - 距今 < `idle_threshold_ms` → 活跃
/// - 否则（含探针不存在/拿不到 mtime）→ 空闲（保守，避免凭空高频轮询）
fn user_is_active(providers: &ProviderRegistry, cfg: &subswap_core::settings::Daemon) -> bool {
    let now = SystemTime::now();
    let mut newest: Option<Duration> = None;
    for p in providers.all() {
        for target in p.client_targets() {
            if let Some(age) = mtime_age(&target.probe_path, now) {
                newest = Some(match newest {
                    Some(prev) if prev <= age => prev,
                    _ => age,
                });
            }
        }
    }
    let idle_threshold = Duration::from_millis(cfg.idle_threshold_ms.max(0) as u64);
    matches!(newest, Some(age) if age < idle_threshold)
}

/// 既有活跃 / 空闲轮询间隔；自适应调度只会在它之上取更早的到期或 reset。
fn decide_next_interval(active: bool, cfg: &subswap_core::settings::Daemon) -> Duration {
    if active {
        Duration::from_millis(cfg.poll_interval_ms)
    } else {
        Duration::from_millis(cfg.idle_poll_interval_ms)
    }
}

fn mtime_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(mtime).ok()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use subswap_core::{
        auto_decide, checkout::Checkout, quota_schedule::ScheduleConfig, Account, AccountId,
        AccountWithQuotas, PolicyConfig, PolicyDecision, ProviderSnapshot, Quota, QuotaCache,
        QuotaFetchState, QuotaPoolSemantics, QuotaReading, QuotaStatus, QuotaWindow,
    };

    use super::{
        add_job, auto_swap_still_allowed, collect_snapshots, merge_collected_state, AdaptiveState,
    };
    use std::collections::{HashMap, VecDeque};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use subswap_core::{AccountRegistry, ClientTarget, Provider, ProviderRegistry};

    fn account(id: &str, active: bool, manual_only: bool) -> Account {
        let mut account = Account {
            provider: "claude".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            extra: Default::default(),
        };
        if manual_only {
            account.extra.insert("manual_only".into(), true.into());
        }
        account
    }

    #[test]
    fn stale_auto_swap_is_rejected_after_manual_switch() {
        let accounts = vec![
            account("oauth", false, false),
            account("deepseek", true, true),
        ];

        assert!(!auto_swap_still_allowed(
            &accounts,
            Some(&AccountId("oauth".into()))
        ));
    }

    #[test]
    fn active_manual_only_account_always_blocks_auto_swap() {
        let accounts = vec![account("deepseek", true, true)];

        assert!(!auto_swap_still_allowed(
            &accounts,
            Some(&AccountId("deepseek".into()))
        ));
    }

    #[test]
    fn unchanged_oauth_active_allows_current_decision() {
        let accounts = vec![account("oauth", true, false)];

        assert!(auto_swap_still_allowed(
            &accounts,
            Some(&AccountId("oauth".into()))
        ));
    }

    #[test]
    fn checkout_marker_does_not_block_auto_swap_decision() {
        let temp = std::env::temp_dir().join(format!(
            "subswap-daemon-checkout-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        let checkout = Checkout::acquire(&temp, "codex", "candidate").unwrap();
        assert!(subswap_core::checkout::is_checked_out(
            &temp,
            "codex",
            "candidate"
        ));
        let snapshot = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                account_with_quota("active", true, 100, QuotaStatus::Exhausted),
                account_with_quota("candidate", false, 0, QuotaStatus::Ok),
            ],
        };
        let policy = PolicyConfig {
            enabled: true,
            threshold: 0.98,
            allow_unknown: false,
            settle_grace_ms: 0,
            manual_hold_ms: 0,
        };

        assert!(matches!(
            auto_decide(&snapshot, &policy),
            PolicyDecision::Swap { to, .. } if to == AccountId("candidate".into())
        ));

        drop(checkout);
        std::fs::remove_dir_all(temp).unwrap();
    }

    fn account_with_quota(
        id: &str,
        active: bool,
        used: u64,
        status: QuotaStatus,
    ) -> AccountWithQuotas {
        AccountWithQuotas {
            account: account(id, active, false),
            quotas: vec![Quota {
                provider: "claude".into(),
                account_id: AccountId(id.into()),
                window: QuotaWindow::FiveHour,
                used,
                limit: 100,
                reset_at: None,
                status,
                note: None,
            }],
            fetch_state: QuotaFetchState::Ready,
        }
    }

    fn sample_quota(id: &str, used: u64) -> Quota {
        Quota {
            provider: "claude".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used,
            limit: 100,
            reset_at: None,
            status: QuotaStatus::Ok,
            note: None,
        }
    }

    #[test]
    fn merged_reading_keeps_original_observation_time() {
        let cfg = ScheduleConfig::from_settings(&subswap_core::settings::Quota::default());
        let now = Utc::now();
        let observed = now - chrono::Duration::minutes(6);
        let cache = QuotaCache::default();
        let reading = QuotaReading {
            quotas: vec![sample_quota("a", 42)],
            observed_at: observed,
        };

        let (quotas, observed_at, state) =
            merge_collected_state(Some(Ok(reading)), &cache, "claude", "a", &cfg, now);
        assert_eq!(observed_at, Some(observed), "must not restamp as now");
        assert_eq!(quotas.len(), 1);
        assert!(
            matches!(state, QuotaFetchState::Stale { cached_at, .. } if cached_at == observed),
            "data older than decision_max_age must be stale, not Ready"
        );
    }

    #[test]
    fn merged_failure_keeps_stale_numbers_but_clears_on_auth_failure() {
        let cfg = ScheduleConfig::from_settings(&subswap_core::settings::Quota::default());
        let now = Utc::now();
        let observed = now - chrono::Duration::seconds(30);
        let mut cache = QuotaCache::default();
        cache.set_at("claude", "a", vec![sample_quota("a", 10)], observed);

        let (quotas, observed_at, state) = merge_collected_state(
            Some(Err("usage returned 429 too many requests".into())),
            &cache,
            "claude",
            "a",
            &cfg,
            now,
        );
        assert_eq!(quotas.len(), 1);
        assert_eq!(observed_at, Some(observed));
        assert!(matches!(state, QuotaFetchState::Stale { .. }));

        let (quotas, observed_at, state) = merge_collected_state(
            Some(Err("re-login required; unauthorized 401".into())),
            &cache,
            "claude",
            "a",
            &cfg,
            now,
        );
        assert!(quotas.is_empty());
        assert_eq!(observed_at, None);
        assert!(matches!(state, QuotaFetchState::Failed(_)));
    }

    #[test]
    fn not_collected_accounts_merge_cache_age_and_failure() {
        let cfg = ScheduleConfig::from_settings(&subswap_core::settings::Quota::default());
        let now = Utc::now();
        let mut cache = QuotaCache::default();
        cache.set_at("claude", "fresh", vec![sample_quota("fresh", 1)], now);
        cache.set_at(
            "claude",
            "old",
            vec![sample_quota("old", 2)],
            now - chrono::Duration::minutes(10),
        );

        let (_, _, fresh) = merge_collected_state(None, &cache, "claude", "fresh", &cfg, now);
        assert_eq!(fresh, QuotaFetchState::Ready);

        let (quotas, observed_at, old) =
            merge_collected_state(None, &cache, "claude", "old", &cfg, now);
        assert_eq!(observed_at, Some(now - chrono::Duration::minutes(10)));
        assert_eq!(quotas.len(), 1);
        assert!(matches!(old, QuotaFetchState::Stale { .. }));

        let (_, _, missing) = merge_collected_state(None, &cache, "claude", "missing", &cfg, now);
        assert_eq!(missing, QuotaFetchState::Loading);
    }

    #[test]
    fn provider_warning_is_throttled_until_cleared() {
        let mut adaptive = AdaptiveState::new();
        assert!(adaptive.should_warn("claude"));
        assert!(
            !adaptive.should_warn("claude"),
            "same provider must not warn every cycle"
        );
        adaptive.clear_warning("claude");
        assert!(adaptive.should_warn("claude"));
    }

    #[test]
    fn job_queue_deduplicates_and_preserves_order() {
        let mut jobs = std::collections::HashMap::new();
        add_job(&mut jobs, "claude", &AccountId("b".into()));
        add_job(&mut jobs, "claude", &AccountId("a".into()));
        add_job(&mut jobs, "claude", &AccountId("b".into()));
        assert_eq!(
            jobs.get("claude").unwrap(),
            &vec![AccountId("b".into()), AccountId("a".into())]
        );
    }

    /// 手工实现 `Provider`（daemon crate 没有 async-trait dev-dependency），只为调度测试
    /// 提供固定的账号列表与按调用次序返回的额度。
    struct TestProvider {
        accounts: Vec<Account>,
        responses: Mutex<HashMap<String, VecDeque<Vec<Quota>>>>,
        calls: Mutex<Vec<String>>,
    }

    impl TestProvider {
        fn new(accounts: Vec<Account>, responses: HashMap<String, Vec<Vec<Quota>>>) -> Self {
            Self {
                accounts,
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(id, queue)| (id, queue.into()))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Provider for TestProvider {
        fn id(&self) -> &'static str {
            "test"
        }

        fn display_name(&self) -> &'static str {
            "Test"
        }

        fn client_targets(&self) -> Vec<ClientTarget> {
            Vec::new()
        }

        fn list_accounts<'life0, 'async_trait>(
            &'life0 self,
        ) -> Pin<Box<dyn Future<Output = subswap_core::Result<Vec<Account>>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Ok(self.accounts.clone()) })
        }

        fn activate<'life0, 'life1, 'async_trait>(
            &'life0 self,
            _id: &'life1 AccountId,
        ) -> Pin<Box<dyn Future<Output = subswap_core::Result<()>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { Ok(()) })
        }

        fn query_quota<'life0, 'life1, 'async_trait>(
            &'life0 self,
            id: &'life1 AccountId,
        ) -> Pin<Box<dyn Future<Output = subswap_core::Result<Vec<Quota>>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            let id = id.0.clone();
            Box::pin(async move {
                self.calls.lock().unwrap().push(id.clone());
                let mut responses = self.responses.lock().unwrap();
                let queue = responses.entry(id).or_default();
                if queue.len() > 1 {
                    Ok(queue.pop_front().unwrap_or_default())
                } else {
                    Ok(queue.front().cloned().unwrap_or_default())
                }
            })
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "subswap-schedule-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn quota_with_reset(
        id: &str,
        used: u64,
        status: QuotaStatus,
        reset_at: Option<chrono::DateTime<Utc>>,
    ) -> Quota {
        Quota {
            provider: "test".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::FiveHour,
            used,
            limit: 100,
            reset_at,
            status,
            note: None,
        }
    }

    #[tokio::test]
    async fn adaptive_collection_uses_classes_and_never_storms() {
        let tmp = temp_dir("cadence");
        let cache_path = tmp.join("quota_cache.json");
        let now = Utc::now();
        let mut cache = QuotaCache::default();
        cache.set_at(
            "test",
            "active",
            vec![quota_with_reset(
                "active",
                10,
                QuotaStatus::Ok,
                Some(now + chrono::Duration::hours(1)),
            )],
            now - chrono::Duration::minutes(4),
        );
        // 停放账号 10 分钟未刷新：超过决策年龄，但仍不到 15 分钟节奏。
        cache.set_at(
            "test",
            "parked",
            vec![quota_with_reset(
                "parked",
                10,
                QuotaStatus::Ok,
                Some(now + chrono::Duration::hours(1)),
            )],
            now - chrono::Duration::minutes(10),
        );
        cache.set_at(
            "test",
            "exhausted",
            vec![quota_with_reset(
                "exhausted",
                100,
                QuotaStatus::Exhausted,
                Some(now + chrono::Duration::hours(1)),
            )],
            now - chrono::Duration::minutes(20),
        );
        cache.save(&cache_path);

        let provider = Arc::new(TestProvider::new(
            vec![
                account("active", true, false),
                account("parked", false, false),
                account("exhausted", false, false),
            ],
            HashMap::new(),
        ));
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let account_registry = AccountRegistry::new(tmp.join("registry.toml"));
        let mut adaptive = AdaptiveState::new();
        let settings = subswap_core::settings::Settings {
            quota: subswap_core::settings::Quota {
                refresh_jitter_pct: 0.0,
                candidate_refresh_limit: 0,
                ..subswap_core::settings::Quota::default()
            },
            ..subswap_core::settings::Settings::default()
        };

        let (snapshots, _) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        let mut calls = provider.calls();
        calls.sort();
        assert_eq!(calls, vec!["active", "exhausted"], "parked must wait 15m");
        let test_snapshot = snapshots
            .iter()
            .find(|snap| snap.provider == "test")
            .unwrap();
        let parked = test_snapshot
            .accounts
            .iter()
            .find(|account| account.account.id.0 == "parked")
            .unwrap();
        assert!(
            matches!(parked.fetch_state, QuotaFetchState::Stale { .. }),
            "10-minute-old data must be stale, not Ready"
        );

        // 紧接着再跑一轮：刚采过的账号在 3m/15m 内不再重复请求。
        let _ = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert_eq!(
            provider.calls().len(),
            2,
            "must not re-query on every cycle"
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[tokio::test]
    async fn known_reset_forces_affected_account_refresh() {
        let tmp = temp_dir("reset");
        let cache_path = tmp.join("quota_cache.json");
        let reset = Utc::now() + chrono::Duration::milliseconds(200);
        let mut responses = HashMap::new();
        responses.insert(
            "active".to_string(),
            vec![
                vec![
                    quota_with_reset("active", 100, QuotaStatus::Exhausted, Some(reset)),
                    Quota {
                        provider: "test".into(),
                        account_id: AccountId("active".into()),
                        window: QuotaWindow::SevenDay,
                        used: 10,
                        limit: 100,
                        reset_at: Some(Utc::now() + chrono::Duration::days(3)),
                        status: QuotaStatus::Ok,
                        note: None,
                    },
                ],
                vec![
                    quota_with_reset(
                        "active",
                        0,
                        QuotaStatus::Ok,
                        Some(Utc::now() + chrono::Duration::hours(5)),
                    ),
                    Quota {
                        provider: "test".into(),
                        account_id: AccountId("active".into()),
                        window: QuotaWindow::SevenDay,
                        used: 10,
                        limit: 100,
                        reset_at: Some(Utc::now() + chrono::Duration::days(3)),
                        status: QuotaStatus::Ok,
                        note: None,
                    },
                ],
            ],
        );
        let provider = Arc::new(TestProvider::new(
            vec![account("active", true, false)],
            responses,
        ));
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let account_registry = AccountRegistry::new(tmp.join("registry.toml"));
        let mut adaptive = AdaptiveState::new();
        let settings = subswap_core::settings::Settings {
            quota: subswap_core::settings::Quota {
                refresh_jitter_pct: 0.0,
                min_refresh_interval_ms: 0,
                reset_refresh_delay_ms: 0,
                candidate_refresh_limit: 0,
                ..subswap_core::settings::Quota::default()
            },
            ..subswap_core::settings::Settings::default()
        };

        let (_, outcome) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert_eq!(provider.calls(), vec!["active"]);
        assert!(
            outcome.next_reset.is_some(),
            "known gating reset should schedule a wake"
        );

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (snapshots, _) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert_eq!(
            provider.calls(),
            vec!["active", "active"],
            "reset + delay must refresh the affected account even though the 3m cadence is not due"
        );
        let active = &snapshots
            .iter()
            .find(|snap| snap.provider == "test")
            .unwrap()
            .accounts[0];
        assert_eq!(active.fetch_state, QuotaFetchState::Ready);
        let _ = std::fs::remove_dir_all(tmp);
    }

    fn quota_window(
        id: &str,
        window: QuotaWindow,
        used: u64,
        status: QuotaStatus,
        reset_at: Option<chrono::DateTime<Utc>>,
    ) -> Quota {
        Quota {
            provider: "test".into(),
            account_id: AccountId(id.into()),
            window,
            used,
            limit: 100,
            reset_at,
            status,
            note: None,
        }
    }

    /// 健康账号（激活与停放）的展示倒计时到期必须刷新；升级前缓存没有协调器记录
    /// 也必须能作为 reset 种子。同一账号多个窗口同一时刻只查询一次。
    #[tokio::test]
    async fn healthy_active_and_parked_reset_expiry_refresh_once() {
        let tmp = temp_dir("healthy-reset");
        let cache_path = tmp.join("quota_cache.json");
        let now = Utc::now();
        let reset = now + chrono::Duration::milliseconds(200);
        let later = now + chrono::Duration::days(3);
        let mut cache = QuotaCache::default();
        cache.set_at(
            "test",
            "active",
            vec![
                quota_window(
                    "active",
                    QuotaWindow::FiveHour,
                    40,
                    QuotaStatus::Ok,
                    Some(reset),
                ),
                quota_window(
                    "active",
                    QuotaWindow::SevenDay,
                    10,
                    QuotaStatus::Ok,
                    Some(later),
                ),
            ],
            now,
        );
        cache.set_at(
            "test",
            "parked",
            vec![
                quota_window(
                    "parked",
                    QuotaWindow::FiveHour,
                    30,
                    QuotaStatus::Ok,
                    Some(reset),
                ),
                quota_window(
                    "parked",
                    QuotaWindow::SevenDay,
                    5,
                    QuotaStatus::Ok,
                    Some(later),
                ),
            ],
            now - chrono::Duration::minutes(1),
        );
        cache.save(&cache_path);

        // 没有 coordinator record：只能靠升级前缓存观测排定 reset。
        assert!(
            subswap_core::quota_collection::read_status_async(&cache_path, "test", "active")
                .await
                .unwrap()
                .is_none()
        );

        let mut responses = HashMap::new();
        responses.insert(
            "active".to_string(),
            vec![vec![
                quota_window(
                    "active",
                    QuotaWindow::FiveHour,
                    0,
                    QuotaStatus::Ok,
                    Some(now + chrono::Duration::hours(5)),
                ),
                quota_window(
                    "active",
                    QuotaWindow::SevenDay,
                    10,
                    QuotaStatus::Ok,
                    Some(later),
                ),
            ]],
        );
        responses.insert(
            "parked".to_string(),
            vec![vec![
                quota_window(
                    "parked",
                    QuotaWindow::FiveHour,
                    0,
                    QuotaStatus::Ok,
                    Some(now + chrono::Duration::hours(5)),
                ),
                quota_window(
                    "parked",
                    QuotaWindow::SevenDay,
                    5,
                    QuotaStatus::Ok,
                    Some(later),
                ),
            ]],
        );
        let provider = Arc::new(TestProvider::new(
            vec![
                account("active", true, false),
                account("parked", false, false),
            ],
            responses,
        ));
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let account_registry = AccountRegistry::new(tmp.join("registry.toml"));
        let mut adaptive = AdaptiveState::new();
        let settings = subswap_core::settings::Settings {
            quota: subswap_core::settings::Quota {
                refresh_jitter_pct: 0.0,
                min_refresh_interval_ms: 0,
                reset_refresh_delay_ms: 0,
                candidate_refresh_limit: 0,
                ..subswap_core::settings::Quota::default()
            },
            ..subswap_core::settings::Settings::default()
        };

        let (_, outcome) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert!(
            provider.calls().is_empty(),
            "healthy accounts are not due yet"
        );
        assert!(
            outcome.next_reset.is_some(),
            "healthy countdown expiry must schedule a wake"
        );

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (snapshots, _) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        let mut calls = provider.calls();
        calls.sort();
        assert_eq!(
            calls,
            vec!["active", "parked"],
            "each healthy account must be queried once at reset expiry"
        );
        let test_snapshot = snapshots
            .iter()
            .find(|snap| snap.provider == "test")
            .unwrap();
        for account in &test_snapshot.accounts {
            assert_eq!(account.fetch_state, QuotaFetchState::Ready);
            assert_eq!(account.quotas.len(), 2, "real returned windows only");
            assert_eq!(account.quotas[0].usage_ratio(), Some(0.0));
        }
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 成功但无额度窗口的账号（自定义 API）也要保留观测时间，不能按 90 秒请求地板反复重采。
    #[tokio::test]
    async fn empty_success_keeps_parked_cadence() {
        let tmp = temp_dir("empty-success");
        let cache_path = tmp.join("quota_cache.json");
        let provider = Arc::new(TestProvider::new(
            vec![account("api", false, true)],
            HashMap::new(),
        ));
        let settings = subswap_core::settings::Settings {
            quota: subswap_core::settings::Quota {
                refresh_jitter_pct: 0.0,
                min_refresh_interval_ms: 0,
                candidate_refresh_limit: 0,
                ..subswap_core::settings::Quota::default()
            },
            ..subswap_core::settings::Settings::default()
        };
        let id = AccountId("api".into());
        let reading = subswap_core::quota_collection::collect_quota_at(
            provider.as_ref(),
            &id,
            &cache_path,
            &settings,
        )
        .await
        .unwrap();
        assert!(reading.quotas.is_empty());
        assert_eq!(provider.calls(), vec!["api"]);

        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let account_registry = AccountRegistry::new(tmp.join("registry.toml"));
        let mut adaptive = AdaptiveState::new();
        let _ = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert_eq!(
            provider.calls(),
            vec!["api"],
            "empty successful reading must keep the parked cadence instead of the request floor"
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// 同一账号不同窗口的后续 reset 在第一次刷新后仍需继续唤醒。
    #[tokio::test]
    async fn following_window_reset_wakes_again_after_first_refresh() {
        let tmp = temp_dir("following-reset");
        let cache_path = tmp.join("quota_cache.json");
        let now = Utc::now();
        let first = now + chrono::Duration::milliseconds(200);
        let second = now + chrono::Duration::milliseconds(500);
        let mut cache = QuotaCache::default();
        cache.set_at(
            "test",
            "active",
            vec![
                quota_window(
                    "active",
                    QuotaWindow::FiveHour,
                    40,
                    QuotaStatus::Ok,
                    Some(first),
                ),
                quota_window(
                    "active",
                    QuotaWindow::SevenDay,
                    10,
                    QuotaStatus::Ok,
                    Some(second),
                ),
            ],
            now,
        );
        cache.save(&cache_path);

        let mut responses = HashMap::new();
        responses.insert(
            "active".to_string(),
            vec![
                vec![
                    quota_window(
                        "active",
                        QuotaWindow::FiveHour,
                        0,
                        QuotaStatus::Ok,
                        Some(now + chrono::Duration::hours(5)),
                    ),
                    quota_window(
                        "active",
                        QuotaWindow::SevenDay,
                        10,
                        QuotaStatus::Ok,
                        Some(second),
                    ),
                ],
                vec![
                    quota_window(
                        "active",
                        QuotaWindow::FiveHour,
                        0,
                        QuotaStatus::Ok,
                        Some(now + chrono::Duration::hours(5)),
                    ),
                    quota_window(
                        "active",
                        QuotaWindow::SevenDay,
                        0,
                        QuotaStatus::Ok,
                        Some(now + chrono::Duration::days(7)),
                    ),
                ],
            ],
        );
        let provider = Arc::new(TestProvider::new(
            vec![account("active", true, false)],
            responses,
        ));
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let account_registry = AccountRegistry::new(tmp.join("registry.toml"));
        let mut adaptive = AdaptiveState::new();
        let settings = subswap_core::settings::Settings {
            quota: subswap_core::settings::Quota {
                refresh_jitter_pct: 0.0,
                min_refresh_interval_ms: 0,
                reset_refresh_delay_ms: 0,
                candidate_refresh_limit: 0,
                ..subswap_core::settings::Quota::default()
            },
            ..subswap_core::settings::Settings::default()
        };

        let (_, outcome) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert!(provider.calls().is_empty());
        assert!(outcome.next_reset.is_some());

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let (_, outcome) = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert_eq!(provider.calls(), vec!["active"]);
        assert!(
            outcome.next_reset.is_some(),
            "the second window must remain scheduled"
        );

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = collect_snapshots(
            &registry,
            &account_registry,
            &mut adaptive,
            &cache_path,
            &settings,
            0.99,
        )
        .await;
        assert_eq!(
            provider.calls(),
            vec!["active", "active"],
            "following window must query exactly once more"
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}
