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

use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use fs2::FileExt;
use subswap_core::{
    auto_decide, paths::AppPaths, query_quota_with_retry, settings, AccountRegistry,
    AccountWithQuotas, AuditEvent, AuditLog, FileStore, KeyringStore, PolicyConfig, PolicyDecision,
    Provider, ProviderRegistry, ProviderSnapshot, QuotaCache, QuotaFetchState,
};
use subswap_provider_claude::ClaudeProvider;
use subswap_provider_codex::CodexProvider;
use subswap_provider_cursor::CursorProvider;
use subswap_provider_kimi::KimiProvider;
use tokio::signal::unix::{signal, SignalKind};

use state::DaemonState;

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
    let audit = AuditLog::from_default_paths()?;

    let claude = Arc::new(ClaudeProvider::new(store.clone(), registry.clone()));
    let codex = Arc::new(subswap_provider_codex::new(store.clone(), registry.clone()));
    let kimi = Arc::new(subswap_provider_kimi::new(store.clone(), registry.clone()));
    let cursor = Arc::new(CursorProvider::new(store.clone(), registry.clone())?);
    let mut providers = ProviderRegistry::new();
    providers.register(claude.clone());
    providers.register(codex.clone());
    providers.register(kimi.clone());
    providers.register(cursor.clone());

    let mut state = DaemonState::new();

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
        };
        if let Err(e) = run_cycle(
            &providers,
            &claude,
            &codex,
            &kimi,
            &cursor,
            &audit,
            &mut state,
            &policy,
            &paths.data_dir,
        )
        .await
        {
            tracing::warn!(err = %e, "daemon cycle failed; will retry next interval");
        }

        // 3. 按「活跃 / 空闲」选下一轮间隔。判定信号：provider 的 probe 文件 mtime。
        let poll = decide_next_interval(&providers, &snapshot_settings.daemon);
        tracing::debug!(
            interval_ms = poll.as_millis() as u64,
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
    claude: &Arc<ClaudeProvider>,
    codex: &Arc<CodexProvider>,
    kimi: &Arc<KimiProvider>,
    cursor: &Arc<CursorProvider>,
    audit: &AuditLog,
    state: &mut DaemonState,
    policy: &PolicyConfig,
    data_dir: &std::path::Path,
) -> Result<()> {
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
    if let Err(e) = cursor.reconcile_active_from_live().await {
        tracing::debug!(err = %e, "cursor live-credential reconcile skipped");
    }

    // (a) 收集每个 Provider 的快照。query_quota 失败的账号 fetch_error 带原因。
    let snapshots = build_snapshots(providers).await;

    // (b) per-provider 跑决策 + 执行。
    for snap in &snapshots {
        if snap.accounts.is_empty() {
            continue;
        }

        // Degraded 期内跳过该 Provider 的 swap(但 token 保活仍要做)。
        if state.is_degraded(&snap.provider) {
            tracing::debug!(provider = %snap.provider, "provider in degraded window; skip");
            continue;
        }
        match auto_decide(snap, policy) {
            PolicyDecision::Swap { from, to, .. } => {
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
            PolicyDecision::NoOp { .. } => {}
        }
    }

    // (c) Claude token 后台保活:对所有 Claude 账号检查 expires_at。
    keep_claude_tokens_alive(claude, data_dir).await;
    Ok(())
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

async fn build_snapshots(providers: &ProviderRegistry) -> Vec<ProviderSnapshot> {
    // 缓存节流:与 CLI 共用 quota_cache.json。缓存够新就复用、不打 usage 端点,把每账号请求频率
    // 压到 ~min_refresh 一次,避免 daemon+CLI 并发查爆端点触发 429。
    let cache_path = AppPaths::resolve().ok().map(|p| p.quota_cache_file());
    let mut cache = cache_path
        .as_ref()
        .map(|p| QuotaCache::load(p))
        .unwrap_or_default();
    let min_refresh =
        std::time::Duration::from_millis(settings::current().quota.min_refresh_interval_ms);

    let mut out = Vec::new();
    for p in providers.all() {
        let provider_id = p.id().to_string();
        let accounts = match p.list_accounts().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(err = %e, provider = %provider_id, "list_accounts failed");
                continue;
            }
        };
        let mut awqs = Vec::with_capacity(accounts.len());
        for account in accounts {
            let id = account.id.clone();
            // 够新就复用缓存,跳过真实查询。
            if let Some(entry) = cache.fresh(&provider_id, &id.0, min_refresh) {
                awqs.push(AccountWithQuotas {
                    account,
                    quotas: entry.quotas,
                    fetch_state: QuotaFetchState::Ready,
                });
                continue;
            }
            let (quotas, fetch_state) = match query_quota_with_retry(p.as_ref(), &id).await {
                Ok(q) => {
                    cache.set(&provider_id, &id.0, q.clone());
                    (q, QuotaFetchState::Ready)
                }
                Err(e) => (Vec::new(), QuotaFetchState::Failed(e.to_string())),
            };
            awqs.push(AccountWithQuotas {
                account,
                quotas,
                fetch_state,
            });
        }
        out.push(ProviderSnapshot {
            provider: provider_id,
            accounts: awqs,
        });
    }
    if let Some(path) = cache_path {
        cache.save(&path);
    }
    out
}

/// 扫 Claude 账号,临近过期的触发 refresh。任一账号失败仅 warn。
///
/// **跳过被隔离会话借走的账号**：该账号的 token 此刻由隔离环境里的 Claude Code 唯一轮换，
/// daemon 再去刷会与之冲突导致 refresh token 被作废（同 active 账号守卫的道理，见
/// docs/design/ACCOUNT_ISOLATION_DESIGN.md §5）。
async fn keep_claude_tokens_alive(claude: &Arc<ClaudeProvider>, data_dir: &std::path::Path) {
    let accounts = match claude.list_accounts().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(err = %e, "claude list_accounts failed during keepalive");
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

/// 根据「最近一次客户端活动」选下一轮轮询间隔。
///
/// 活动信号：所有 provider `client_targets().probe_path` 中最近一次 mtime。
/// - 距今 < `idle_threshold_ms` → 活跃，用 `poll_interval_ms`
/// - 否则 → 空闲，用 `idle_poll_interval_ms`
///
/// 探针文件不存在 / 拿不到 mtime 时按「空闲」处理（保守，避免凭空高频轮询）。
fn decide_next_interval(
    providers: &ProviderRegistry,
    cfg: &subswap_core::settings::Daemon,
) -> Duration {
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
    match newest {
        Some(age) if age < idle_threshold => Duration::from_millis(cfg.poll_interval_ms),
        _ => Duration::from_millis(cfg.idle_poll_interval_ms),
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
        auto_decide, checkout::Checkout, Account, AccountId, AccountWithQuotas, PolicyConfig,
        PolicyDecision, ProviderSnapshot, Quota, QuotaFetchState, QuotaStatus, QuotaWindow,
    };

    use super::auto_swap_still_allowed;

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
}
